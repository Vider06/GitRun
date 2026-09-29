//! Kernel-backed GSR execution supervisor for Linux runner containers.
//!
//! Layer 1 (the shell agent) is intentionally cheap and catches ordinary
//! GitHub Actions run steps before the shell starts. This module is the
//! stronger boundary underneath it: the Actions runner process and every
//! descendant are ptrace-traced, and a seccomp filter turns every execve
//! and execveat into a synchronous supervisor stop before the kernel starts
//! the new program.
//!
//! This closes two important gaps in the old design:
//! - a job cannot bypass the shell agent simply by calling execve from
//!   Python/Node/a compiled helper;
//! - a short-lived process cannot disappear between two docker top polls.
//!
//! The supervisor itself remains PID 1/root only long enough to own the
//! ptrace boundary. The actual Actions runner child drops to the dedicated
//! runner uid/gid before installing the seccomp filter and executing
//! run.sh. PTRACE_O_EXITKILL makes the traced process tree die if the
//! supervisor disappears.
//!
//! Linux/x86_64 is the production target because GitRun's runner image is
//! currently the official x86_64 Actions Runner artifact. Other platforms
//! deliberately fail closed instead of silently falling back to the weaker
//! shell-only implementation.

use gitrun_core::command_policy::CommandPolicy;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use gitrun_core::command_policy::Decision;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ffi::{CStr, CString};
use std::path::Path;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("GSR exec supervisor is only supported on Linux x86_64")]
    UnsupportedPlatform,
    #[error("runner user 'runner' does not exist")]
    RunnerUserMissing,
    #[error("invalid runner user metadata")]
    InvalidRunnerUser,
    #[error("system call failed: {0}: {1}")]
    Syscall(&'static str, std::io::Error),
    #[error("ptrace setup failed: {0}")]
    Ptrace(&'static str),
    #[error("exec inspection failed for pid {0}")]
    InspectFailed(i32),
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn run(
    _events_path: &Path,
    _policy: &CommandPolicy,
) -> Result<i32, SupervisorError> {
    Err(SupervisorError::UnsupportedPlatform)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use std::collections::HashSet;

    const RUNNER_USER: &str = "runner";
    const RUNNER_SCRIPT: &str = "/home/runner/actions-runner/run.sh";

    const PTRACE_TRACEME: libc::c_uint = 0;
    const PTRACE_CONT: libc::c_uint = 7;
    const PTRACE_GETREGS: libc::c_uint = 12;
    const PTRACE_SETREGS: libc::c_uint = 13;
    const PTRACE_SETOPTIONS: libc::c_uint = 0x4200;
    const PTRACE_GETEVENTMSG: libc::c_uint = 0x4201;

    const PTRACE_O_TRACEFORK: libc::c_ulong = 0x0000_0002;
    const PTRACE_O_TRACEVFORK: libc::c_ulong = 0x0000_0004;
    const PTRACE_O_TRACECLONE: libc::c_ulong = 0x0000_0008;
    const PTRACE_O_TRACEEXEC: libc::c_ulong = 0x0000_0010;
    const PTRACE_O_TRACESECCOMP: libc::c_ulong = 0x0000_0080;
    const PTRACE_O_EXITKILL: libc::c_ulong = 0x0010_0000;

    const PTRACE_EVENT_FORK: libc::c_ulong = 1;
    const PTRACE_EVENT_VFORK: libc::c_ulong = 2;
    const PTRACE_EVENT_CLONE: libc::c_ulong = 3;
    const PTRACE_EVENT_EXEC: libc::c_ulong = 4;
    const PTRACE_EVENT_SECCOMP: libc::c_ulong = 7;

    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_K: u16 = 0x00;
    const BPF_RET: u16 = 0x06;

    const SECCOMP_DATA_NR_OFFSET: u32 = 0;
    const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

    const MAX_ARGC: usize = 256;
    const MAX_STRING: usize = 4096;

    #[derive(Clone)]
    struct RunnerIdentity {
        uid: libc::uid_t,
        gid: libc::gid_t,
        name: CString,
    }

    /// Runs the Actions runner as an unprivileged tracee and supervises all
    /// descendant execs synchronously in the parent.
    pub(super) fn run(
        events_path: &Path,
        policy: &CommandPolicy,
    ) -> Result<i32, SupervisorError> {
        let runner = lookup_runner()?;
        let stopping = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())
            .map_err(|error| SupervisorError::Syscall("register SIGTERM", error))?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())
            .map_err(|error| SupervisorError::Syscall("register SIGINT", error))?;

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(SupervisorError::Syscall(
                "fork",
                std::io::Error::last_os_error(),
            ));
        }

        if pid == 0 {
            child_main(&runner);
        }

        supervise_parent(pid, policy, events_path, stopping)
    }

    fn lookup_runner() -> Result<RunnerIdentity, SupervisorError> {
        let username = CString::new(RUNNER_USER).expect("constant contains no NUL");
        let passwd = unsafe { libc::getpwnam(username.as_ptr()) };
        if passwd.is_null() {
            return Err(SupervisorError::RunnerUserMissing);
        }
        let entry = unsafe { *passwd };
        if entry.pw_uid == 0 || entry.pw_gid == 0 || entry.pw_name.is_null() {
            return Err(SupervisorError::InvalidRunnerUser);
        }
        let name = unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned();
        Ok(RunnerIdentity {
            uid: entry.pw_uid,
            gid: entry.pw_gid,
            name: CString::new(name).map_err(|_| SupervisorError::InvalidRunnerUser)?,
        })
    }

    /// Child-side bootstrap. Any failure uses _exit so the child does not
    /// run parent-side destructors after fork.
    fn child_main(runner: &RunnerIdentity) -> ! {
        unsafe {
            if libc::setpgid(0, 0) != 0 {
                libc::_exit(125);
            }
            // Protect against the tiny pre-PTRACE_O_EXITKILL window if the
            // PID-1 supervisor dies before it can configure ptrace options.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                libc::_exit(125);
            }

            // TRACEME must happen while the child still has the container's
            // root capability set; the runner uid deliberately has no
            // CAP_SYS_PTRACE.
            if libc::ptrace(PTRACE_TRACEME, 0, 0, 0) == -1 {
                libc::_exit(125);
            }
            if libc::raise(libc::SIGSTOP) != 0 {
                libc::_exit(125);
            }

            // The parent sets ptrace options before resuming us. We can now
            // drop identity permanently.
            if libc::initgroups(runner.name.as_ptr(), runner.gid) != 0
                || libc::setresgid(runner.gid, runner.gid, runner.gid) != 0
                || libc::setresuid(runner.uid, runner.uid, runner.uid) != 0
            {
                libc::_exit(125);
            }

            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                libc::_exit(125);
            }
            if install_seccomp_trace_filter() != 0 {
                libc::_exit(125);
            }
        }

        let mut command = std::process::Command::new(RUNNER_SCRIPT);
        command.env_clear();
        for (key, value) in std::env::vars_os() {
            command.env(key, value);
        }
        let error = std::os::unix::process::CommandExt::exec(&mut command);
        let _ = error;
        unsafe { libc::_exit(126) }
    }

    fn install_seccomp_trace_filter() -> libc::c_int {
        let filter = [
            libc::sock_filter {
                code: BPF_LD | BPF_W | BPF_ABS,
                jt: 0,
                jf: 0,
                k: SECCOMP_DATA_ARCH_OFFSET,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 1,
                jf: 0,
                k: AUDIT_ARCH_X86_64,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_KILL_PROCESS,
            },
            libc::sock_filter {
                code: BPF_LD | BPF_W | BPF_ABS,
                jt: 0,
                jf: 0,
                k: SECCOMP_DATA_NR_OFFSET,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 2,
                jf: 0,
                k: libc::SYS_execve as u32,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 1,
                jf: 0,
                k: libc::SYS_execveat as u32,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ALLOW,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_TRACE,
            },
        ];

        let program = libc::sock_fprog {
            len: filter.len() as libc::c_ushort,
            filter: filter.as_ptr() as *mut libc::sock_filter,
        };

        unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
                0,
                0,
            )
        }
    }

    fn supervise_parent(
        root_pid: libc::pid_t,
        policy: &CommandPolicy,
        events_path: &Path,
        stopping: Arc<AtomicBool>,
    ) -> Result<i32, SupervisorError> {
        let wait_result = wait_for_initial_stop(root_pid)?;
        if !is_stopped(wait_result) {
            return Err(SupervisorError::Ptrace("root tracee did not stop for setup"));
        }

        let options = PTRACE_O_TRACEFORK
            | PTRACE_O_TRACEVFORK
            | PTRACE_O_TRACECLONE
            | PTRACE_O_TRACEEXEC
            | PTRACE_O_TRACESECCOMP
            | PTRACE_O_EXITKILL;

        if ptrace_setoptions(root_pid, options) == -1 {
            kill_process_group(root_pid);
            return Err(SupervisorError::Ptrace("PTRACE_SETOPTIONS"));
        }
        continue_tracee(root_pid)?;

        let mut tracees = HashSet::from([root_pid]);
        let mut root_exit: Option<i32> = None;

        loop {
            if stopping.load(Ordering::Relaxed) {
                terminate_process_group(root_pid);
            }

            let mut status = 0;
            let waited = unsafe { libc::waitpid(-1, &mut status, libc::WUNTRACED) };
            if waited == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                kill_process_group(root_pid);
                return Err(SupervisorError::Syscall("waitpid", error));
            }

            let pid = waited;

            if is_exited(status) {
                tracees.remove(&pid);
                if pid == root_pid {
                    root_exit = Some((status >> 8) & 0xff);
                    kill_process_group(root_pid);
                }
                if tracees.is_empty() {
                    break;
                }
                continue;
            }

            if is_signaled(status) {
                tracees.remove(&pid);
                if pid == root_pid {
                    root_exit = Some(128 + (status & 0x7f));
                    kill_process_group(root_pid);
                }
                if tracees.is_empty() {
                    break;
                }
                continue;
            }

            if !is_stopped(status) {
                continue;
            }

            let signal = status & 0x7f;
            let event = (status >> 16) & 0xffff;

            if event == PTRACE_EVENT_SECCOMP as i32 {
                match inspect_exec(pid, policy, events_path)? {
                    ExecDecision::Allow => continue_tracee(pid)?,
                    ExecDecision::Deny => deny_and_continue(pid)?,
                }
                continue;
            }

            if matches!(
                event as libc::c_ulong,
                PTRACE_EVENT_FORK | PTRACE_EVENT_VFORK | PTRACE_EVENT_CLONE
            ) {
                let new_pid = ptrace_event_pid(pid)?;
                tracees.insert(new_pid);
                continue_tracee(new_pid)?;
                continue_tracee(pid)?;
                continue;
            }

            if signal == libc::SIGTRAP && event == PTRACE_EVENT_EXEC as i32 {
                continue_tracee(pid)?;
            } else if signal == libc::SIGSTOP || signal == libc::SIGTRAP {
                continue_tracee(pid)?;
            } else {
                continue_tracee_with_signal(pid, signal)?;
            }
        }

        Ok(root_exit.unwrap_or(1))
    }

    fn wait_for_initial_stop(pid: libc::pid_t) -> Result<i32, SupervisorError> {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if waited != pid {
            return Err(SupervisorError::Syscall(
                "waitpid",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(status)
    }

    fn ptrace_setoptions(pid: libc::pid_t, options: libc::c_ulong) -> libc::c_long {
        unsafe { libc::ptrace(PTRACE_SETOPTIONS, pid, 0, options) }
    }

    fn continue_tracee(pid: libc::pid_t) -> Result<(), SupervisorError> {
        continue_tracee_with_signal(pid, 0)
    }

    fn continue_tracee_with_signal(
        pid: libc::pid_t,
        signal: libc::c_int,
    ) -> Result<(), SupervisorError> {
        let result = unsafe { libc::ptrace(PTRACE_CONT, pid, 0, signal) };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_CONT"));
        }
        Ok(())
    }

    fn ptrace_event_pid(pid: libc::pid_t) -> Result<libc::pid_t, SupervisorError> {
        let mut value: libc::c_ulong = 0;
        let result = unsafe {
            libc::ptrace(
                PTRACE_GETEVENTMSG,
                pid,
                0,
                &mut value as *mut libc::c_ulong,
            )
        };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_GETEVENTMSG"));
        }
        Ok(value as libc::pid_t)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExecDecision {
        Allow,
        Deny,
    }

    fn inspect_exec(
        pid: libc::pid_t,
        policy: &CommandPolicy,
        events_path: &Path,
    ) -> Result<ExecDecision, SupervisorError> {
        let mut registers = std::mem::MaybeUninit::<libc::user_regs_struct>::uninit();
        let result = unsafe {
            libc::ptrace(
                PTRACE_GETREGS,
                pid,
                0,
                registers.as_mut_ptr(),
            )
        };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_GETREGS"));
        }
        let registers = unsafe { registers.assume_init() };

        let (path_ptr, argv_ptr) = if registers.orig_rax == libc::SYS_execve as u64 {
            (registers.rdi, registers.rsi)
        } else if registers.orig_rax == libc::SYS_execveat as u64 {
            (registers.rsi, registers.rdx)
        } else {
            return Err(SupervisorError::InspectFailed(pid));
        };

        let path = if path_ptr == 0 {
            "<execveat-empty>".to_owned()
        } else {
            match read_c_string(pid, path_ptr) {
                Ok(value) => value,
                Err(_) => {
                    emit_inspection_failure(events_path, pid, "could not read executable path");
                    return Ok(ExecDecision::Deny);
                }
            }
        };

        let argv = match read_argv(pid, argv_ptr) {
            Ok(value) => value,
            Err(_) => {
                emit_inspection_failure(events_path, pid, "could not read executable arguments");
                return Ok(ExecDecision::Deny);
            }
        };

        let command_line = std::iter::once(path.as_str())
            .chain(argv.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");

        match policy.evaluate(&command_line) {
            Decision::Allowed => Ok(ExecDecision::Allow),
            Decision::Denied { reason } => {
                let event = gitrun_gsr::SecurityEvent::new(
                    "gsr-exec-supervisor",
                    gitrun_gsr::Severity::Critical,
                    format!("blocked execve for pid {pid}: {command_line:?} — {reason}"),
                );
                let _ = gitrun_gsr::events::emit(events_path, &event);
                Ok(ExecDecision::Deny)
            }
        }
    }

    fn deny_and_continue(pid: libc::pid_t) -> Result<(), SupervisorError> {
        let mut registers = std::mem::MaybeUninit::<libc::user_regs_struct>::uninit();
        let result = unsafe {
            libc::ptrace(
                PTRACE_GETREGS,
                pid,
                0,
                registers.as_mut_ptr(),
            )
        };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_GETREGS"));
        }
        let mut registers = unsafe { registers.assume_init() };

        // Replace the syscall with an invalid number and supply EPERM.
        // seccomp TRACE permits the ptracer to modify the tracee registers
        // before resuming it, so the dangerous executable is never started.
        registers.orig_rax = u64::MAX;
        registers.rax = (-libc::EPERM) as i64 as u64;

        let result = unsafe {
            libc::ptrace(
                PTRACE_SETREGS,
                pid,
                0,
                &registers as *const libc::user_regs_struct,
            )
        };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_SETREGS"));
        }
        continue_tracee(pid)
    }

    fn read_c_string(pid: libc::pid_t, address: u64) -> Result<String, SupervisorError> {
        let mut buffer = vec![0u8; MAX_STRING];
        let bytes = read_process_memory(pid, address, &mut buffer)?;
        let Some(end) = buffer[..bytes].iter().position(|byte| *byte == 0) else {
            return Err(SupervisorError::InspectFailed(pid));
        };
        Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }

    fn read_argv(
        pid: libc::pid_t,
        address: u64,
    ) -> Result<Vec<String>, SupervisorError> {
        if address == 0 {
            return Ok(Vec::new());
        }

        const CHUNK: usize = 32;
        const POINTER_SIZE: u64 = std::mem::size_of::<u64>() as u64;
        let mut args = Vec::new();
        let mut total_bytes = 0usize;

        for chunk_index in 0..(MAX_ARGC / CHUNK) {
            let mut pointers = vec![0u64; CHUNK];
            let remote_address = address + (chunk_index * CHUNK) as u64 * POINTER_SIZE;
            let bytes = read_process_memory(
                pid,
                remote_address,
                unsafe {
                    std::slice::from_raw_parts_mut(
                        pointers.as_mut_ptr() as *mut u8,
                        pointers.len() * std::mem::size_of::<u64>(),
                    )
                },
            )?;

            if bytes == 0 || bytes % std::mem::size_of::<u64>() != 0 {
                return Err(SupervisorError::InspectFailed(pid));
            }

            let pointer_count = bytes / std::mem::size_of::<u64>();
            for &pointer in &pointers[..pointer_count] {
                if pointer == 0 {
                    return Ok(args);
                }
                let value = read_c_string(pid, pointer)?;
                total_bytes = total_bytes.saturating_add(value.len());
                if total_bytes > 64 * 1024 {
                    return Err(SupervisorError::InspectFailed(pid));
                }
                args.push(value);
            }

            if bytes < CHUNK * std::mem::size_of::<u64>() {
                return Err(SupervisorError::InspectFailed(pid));
            }
        }

        Err(SupervisorError::InspectFailed(pid))
    }

    fn read_process_memory(
        pid: libc::pid_t,
        address: u64,
        buffer: &mut [u8],
    ) -> Result<usize, SupervisorError> {
        let mut local = libc::iovec {
            iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
            iov_len: buffer.len(),
        };
        let remote = libc::iovec {
            iov_base: address as *mut libc::c_void,
            iov_len: buffer.len(),
        };
        let count = unsafe {
            libc::process_vm_readv(
                pid,
                &mut local as *mut libc::iovec,
                1,
                &remote as *const libc::iovec,
                1,
                0,
            )
        };
        if count < 0 {
            return Err(SupervisorError::InspectFailed(pid));
        }
        Ok(count as usize)
    }

    fn terminate_process_group(root_pid: libc::pid_t) {
        unsafe {
            libc::kill(-root_pid, libc::SIGTERM);
        }
    }

    fn kill_process_group(root_pid: libc::pid_t) {
        unsafe {
            libc::kill(-root_pid, libc::SIGKILL);
        }
    }

    fn is_stopped(status: i32) -> bool {
        (status & 0xff) == 0x7f
    }

    fn is_exited(status: i32) -> bool {
        (status & 0x7f) == 0
    }

    fn is_signaled(status: i32) -> bool {
        let signal = status & 0x7f;
        signal != 0 && signal != 0x7f
    }
}

pub fn run_supervisor(
    events_path: &Path,
    policy: &CommandPolicy,
) -> Result<i32, SupervisorError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return linux::run(events_path, policy);
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        run(events_path, policy)
    }
}
