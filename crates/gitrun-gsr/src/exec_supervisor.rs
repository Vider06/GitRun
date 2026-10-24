//! Kernel-backed GSR execution supervisor for Linux runner containers.
//!
//! Layer 1 (the shell agent) is intentionally cheap and catches ordinary
//! GitHub Actions run steps before the shell starts. This module is the
//! stronger boundary underneath it: the Actions runner process and every
//! descendant are ptrace-traced, and a seccomp filter turns every execve
//! and execveat into a synchronous supervisor stop before the kernel starts
//! the new program. All threads/tracees are quiesced while the supervisor
//! reads path/argv and until the kernel has consumed them. Non-UTF-8 paths or
//! argv are rejected rather than lossy-normalized before policy evaluation.
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
pub fn run(_events_path: &Path, _policy: &CommandPolicy) -> Result<i32, SupervisorError> {
    Err(SupervisorError::UnsupportedPlatform)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use std::collections::{HashSet, VecDeque};
    use std::os::fd::RawFd;

    const RUNNER_USER: &str = "runner";
    const RUNNER_SCRIPT: &str = "/home/runner/actions-runner/run.sh";

    const PTRACE_CONT: libc::c_uint = 7;
    const PTRACE_GETREGS: libc::c_uint = 12;
    const PTRACE_SETREGS: libc::c_uint = 13;
    const PTRACE_SYSCALL: libc::c_uint = 24;
    const PTRACE_SEIZE: libc::c_uint = 0x4206;
    const PTRACE_INTERRUPT: libc::c_uint = 0x4207;
    const PTRACE_GETEVENTMSG: libc::c_uint = 0x4201;

    const PTRACE_O_TRACEFORK: libc::c_ulong = 0x0000_0002;
    const PTRACE_O_TRACEVFORK: libc::c_ulong = 0x0000_0004;
    const PTRACE_O_TRACECLONE: libc::c_ulong = 0x0000_0008;
    const PTRACE_O_TRACEEXEC: libc::c_ulong = 0x0000_0010;
    const PTRACE_O_TRACESECCOMP: libc::c_ulong = 0x0000_0080;
    const PTRACE_O_TRACESYSGOOD: libc::c_ulong = 0x0000_0001;
    const PTRACE_O_EXITKILL: libc::c_ulong = 0x0010_0000;

    const PTRACE_EVENT_FORK: libc::c_ulong = 1;
    const PTRACE_EVENT_VFORK: libc::c_ulong = 2;
    const PTRACE_EVENT_CLONE: libc::c_ulong = 3;
    const PTRACE_EVENT_EXEC: libc::c_ulong = 4;
    const PTRACE_EVENT_SECCOMP: libc::c_ulong = 7;
    const PTRACE_EVENT_STOP: libc::c_ulong = 128;
    const SYSCALL_STOP_SIGNAL: i32 = libc::SIGTRAP | 0x80;

    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const CLONE_UNTRACED_FLAG: u32 = 0x0080_0000;
    const SECCOMP_DATA_ARGS_OFFSET: u32 = 16;
    // x32 shares AUDIT_ARCH_X86_64 but sets this bit in syscall numbers.
    // Refuse that ABI rather than letting its exec syscalls bypass tracing.
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;

    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_JSET: u16 = 0x40;
    const BPF_K: u16 = 0x00;
    const BPF_RET: u16 = 0x06;

    const SECCOMP_DATA_NR_OFFSET: u32 = 0;
    const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

    const MAX_ARGC: usize = 256;
    const MAX_STRING: usize = 4096;
    const EXECVEAT_AT_EMPTY_PATH: u64 = 0x1000;

    #[derive(Clone)]
    struct RunnerIdentity {
        uid: libc::uid_t,
        gid: libc::gid_t,
        name: CString,
    }

    /// Runs the Actions runner as an unprivileged tracee and supervises all
    /// descendant execs synchronously in the parent.
    pub(super) fn run(events_path: &Path, policy: &CommandPolicy) -> Result<i32, SupervisorError> {
        let runner = lookup_runner()?;
        let stopping = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())
            .map_err(|error| SupervisorError::Syscall("register SIGTERM", error))?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())
            .map_err(|error| SupervisorError::Syscall("register SIGINT", error))?;

        // The child blocks on this pipe until it has been seized. That avoids
        // the TRACEME/SIGSTOP bootstrap and makes PTRACE_INTERRUPT available
        // for every descendant and thread we need to freeze for exec checks.
        let mut gate = [-1; 2];
        if unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
            return Err(SupervisorError::Syscall(
                "create ptrace bootstrap pipe",
                std::io::Error::last_os_error(),
            ));
        }

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(gate[0]);
                libc::close(gate[1]);
            }
            return Err(SupervisorError::Syscall(
                "fork",
                std::io::Error::last_os_error(),
            ));
        }

        if pid == 0 {
            unsafe { libc::close(gate[1]) };
            child_main(&runner, gate[0]);
        }

        unsafe { libc::close(gate[0]) };
        let options = ptrace_options();
        if unsafe { libc::ptrace(PTRACE_SEIZE, pid, 0, options) } == -1 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::close(gate[1]);
            }
            return Err(SupervisorError::Syscall("PTRACE_SEIZE root tracee", error));
        }
        if unsafe { libc::ptrace(PTRACE_INTERRUPT, pid, 0, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::close(gate[1]);
            }
            return Err(SupervisorError::Syscall(
                "PTRACE_INTERRUPT root tracee",
                error,
            ));
        }
        let wait_result = match wait_for_initial_stop(pid) {
            Ok(status) => status,
            Err(error) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::close(gate[1]);
                }
                return Err(error);
            }
        };
        if !is_stopped(wait_result) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::close(gate[1]);
            }
            return Err(SupervisorError::Ptrace(
                "root tracee did not stop after seize",
            ));
        }

        let ready = [1u8];
        let written = unsafe { libc::write(gate[1], ready.as_ptr().cast(), ready.len()) };
        unsafe { libc::close(gate[1]) };
        if written != ready.len() as isize {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return Err(SupervisorError::Syscall(
                "release ptrace bootstrap pipe",
                std::io::Error::last_os_error(),
            ));
        }

        continue_tracee(pid)?;
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
    fn child_main(runner: &RunnerIdentity, gate_read: RawFd) -> ! {
        unsafe {
            if libc::setpgid(0, 0) != 0 {
                libc::_exit(125);
            }
            // Protect against the tiny pre-PTRACE_EXITKILL window if the
            // PID-1 supervisor dies before ptrace options are installed.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                libc::_exit(125);
            }

            // Do not perform any runner bootstrap until the parent has seized
            // and stopped us. PTRACE_SEIZE options then cover the process tree.
            let mut ready = 0u8;
            let read_result = libc::read(gate_read, (&mut ready as *mut u8).cast(), 1);
            libc::close(gate_read);
            if read_result != 1 || ready != 1 {
                libc::_exit(125);
            }

            // The parent has installed PTRACE_O_EXITKILL and resumed us. We can
            // now drop identity permanently.
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
                // The x32 ABI uses the x86-64 audit architecture with a
                // syscall-number bit that would otherwise miss every rule below.
                code: BPF_JMP | BPF_JSET | BPF_K,
                jt: 0,
                jf: 1,
                k: X32_SYSCALL_BIT,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 9,
                jf: 0,
                k: libc::SYS_execve as u32,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 8,
                jf: 0,
                k: libc::SYS_execveat as u32,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 2,
                jf: 0,
                k: libc::SYS_clone as u32,
            },
            libc::sock_filter {
                // clone3 puts its flags behind a user pointer, which classic
                // BPF cannot dereference. Return ENOSYS so callers can use
                // clone instead, where CLONE_UNTRACED can be checked here.
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 5,
                jf: 0,
                k: libc::SYS_clone3 as u32,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ALLOW,
            },
            libc::sock_filter {
                code: BPF_LD | BPF_W | BPF_ABS,
                jt: 0,
                jf: 0,
                k: SECCOMP_DATA_ARGS_OFFSET,
            },
            libc::sock_filter {
                code: BPF_JMP | BPF_JSET | BPF_K,
                jt: 1,
                jf: 0,
                k: CLONE_UNTRACED_FLAG,
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
                k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
            },
            libc::sock_filter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
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
        let mut tracees = HashSet::from([root_pid]);
        let mut root_exit: Option<i32> = None;
        let mut deferred_exits = VecDeque::<(libc::pid_t, i32)>::new();

        loop {
            if stopping.load(Ordering::Relaxed) {
                terminate_process_group(root_pid);
                kill_tracees(&tracees);
            }

            let (pid, status) = if let Some((pid, status)) = deferred_exits.pop_front() {
                (pid, status)
            } else {
                let mut status = 0;
                let waited =
                    unsafe { libc::waitpid(-1, &mut status, libc::WUNTRACED | libc::__WALL) };
                if waited == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    kill_process_group(root_pid);
                    return Err(SupervisorError::Syscall("waitpid", error));
                }
                (waited, status)
            };

            if is_exited(status) {
                tracees.remove(&pid);
                if pid == root_pid {
                    root_exit = Some((status >> 8) & 0xff);
                    kill_process_group(root_pid);
                    kill_tracees(&tracees);
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
                    kill_tracees(&tracees);
                }
                if tracees.is_empty() {
                    break;
                }
                continue;
            }

            if !is_stopped(status) {
                continue;
            }

            let signal = (status >> 8) & 0xff;
            let event = (status >> 16) & 0xffff;

            if event == PTRACE_EVENT_SECCOMP as i32 {
                // Snapshot all tracees before reading path/argv, then keep them
                // stopped until the kernel has consumed these pointers. The
                // syscall-exit stop handles failed execve; PTRACE_EVENT_EXEC
                // handles successful execve.
                let mut frozen =
                    freeze_tracees(pid, &mut tracees, &mut deferred_exits, events_path)?;
                match inspect_exec(pid, policy, events_path)? {
                    ExecDecision::Deny => {
                        deny_exec_syscall(pid)?;
                        thaw_tracees(&mut frozen, &mut tracees)?;
                        continue_tracee(pid)?;
                    }
                    ExecDecision::Allow => {
                        match complete_inspected_exec(
                            pid,
                            &mut tracees,
                            &mut frozen,
                            &mut deferred_exits,
                            events_path,
                        )? {
                            ExecCompletion::SyscallReturned(returned_pid) => {
                                thaw_tracees(&mut frozen, &mut tracees)?;
                                resume_if_alive(returned_pid, 0)?;
                            }
                            ExecCompletion::Executed {
                                pid: exec_pid,
                                former_tid,
                            } => {
                                if former_tid != exec_pid {
                                    tracees.remove(&former_tid);
                                    // A non-leader exec adopts the TGID as its
                                    // new PID. Do not try to resume the former
                                    // leader a second time with a stale signal.
                                    frozen.resume.retain(|(held_pid, _)| *held_pid != exec_pid);
                                }
                                tracees.insert(exec_pid);
                                thaw_tracees(&mut frozen, &mut tracees)?;
                                resume_if_alive(exec_pid, 0)?;
                            }
                            ExecCompletion::TraceeExited => {
                                thaw_tracees(&mut frozen, &mut tracees)?;
                            }
                        }
                    }
                }
                continue;
            }

            if matches!(
                event as libc::c_ulong,
                PTRACE_EVENT_FORK | PTRACE_EVENT_VFORK | PTRACE_EVENT_CLONE
            ) {
                let new_pid = ptrace_event_pid(pid)?;
                tracees.insert(new_pid);
                // The child has its own initial PTRACE_EVENT_STOP. We only
                // resume it after that stop is collected and registered.
                continue_tracee(pid)?;
                continue;
            }

            if signal == libc::SIGSTOP || signal == libc::SIGTRAP {
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

    fn ptrace_options() -> libc::c_ulong {
        PTRACE_O_TRACEFORK
            | PTRACE_O_TRACEVFORK
            | PTRACE_O_TRACECLONE
            | PTRACE_O_TRACEEXEC
            | PTRACE_O_TRACESECCOMP
            | PTRACE_O_TRACESYSGOOD
            | PTRACE_O_EXITKILL
    }

    #[derive(Default)]
    struct FrozenTracees {
        // Tracees stopped by PTRACE_INTERRUPT, plus event stops we consumed
        // while freezing. Each entry stores the signal to deliver on resume.
        resume: Vec<(libc::pid_t, libc::c_int)>,
    }

    enum ExecCompletion {
        SyscallReturned(libc::pid_t),
        Executed {
            pid: libc::pid_t,
            former_tid: libc::pid_t,
        },
        TraceeExited,
    }

    fn freeze_tracees(
        target: libc::pid_t,
        tracees: &mut HashSet<libc::pid_t>,
        deferred_exits: &mut VecDeque<(libc::pid_t, i32)>,
        events_path: &Path,
    ) -> Result<FrozenTracees, SupervisorError> {
        let mut frozen = FrozenTracees::default();

        loop {
            let covered: HashSet<_> = frozen.resume.iter().map(|(pid, _)| *pid).collect();
            let pending: Vec<_> = tracees
                .iter()
                .copied()
                .filter(|pid| *pid != target && !covered.contains(pid))
                .collect();
            if pending.is_empty() {
                return Ok(frozen);
            }

            for pid in pending {
                let result = unsafe { libc::ptrace(PTRACE_INTERRUPT, pid, 0, 0) };
                if result == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        // The tracee has already exited. Collect its wait status
                        // if available; it can no longer mutate the target's memory.
                        let mut status = 0;
                        let waited = unsafe {
                            libc::waitpid(pid, &mut status, libc::__WALL | libc::WNOHANG)
                        };
                        if waited == pid {
                            tracees.remove(&pid);
                            deferred_exits.push_back((pid, status));
                        } else if waited == 0 {
                            return Err(SupervisorError::Ptrace(
                                "tracee vanished while freezing exec arguments",
                            ));
                        } else {
                            let wait_error = std::io::Error::last_os_error();
                            if wait_error.raw_os_error() == Some(libc::EINTR) {
                                continue;
                            }
                            return Err(SupervisorError::Syscall(
                                "waitpid vanished tracee",
                                wait_error,
                            ));
                        }
                        continue;
                    }
                    return Err(SupervisorError::Syscall("PTRACE_INTERRUPT", error));
                }
            }

            let mut status = 0;
            let waited = unsafe { libc::waitpid(-1, &mut status, libc::WUNTRACED | libc::__WALL) };
            if waited == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(SupervisorError::Syscall(
                    "waitpid while freezing tracees",
                    error,
                ));
            }
            let pid = waited;

            if is_exited(status) || is_signaled(status) {
                tracees.remove(&pid);
                deferred_exits.push_back((pid, status));
                continue;
            }
            if !is_stopped(status) {
                continue;
            }
            if pid == target {
                return Err(SupervisorError::Ptrace(
                    "exec tracee stopped unexpectedly while freezing peers",
                ));
            }

            tracees.insert(pid);
            let signal = (status >> 8) & 0xff;
            let event = (status >> 16) & 0xffff;
            if event == PTRACE_EVENT_SECCOMP as i32 {
                // Two threads can reach execve's seccomp stop concurrently.
                // Only the first inspected syscall is eligible to run while
                // the rest are frozen; deny the second without reading mutable
                // arguments so it cannot race the active authorization.
                deny_exec_syscall(pid)?;
                emit_inspection_failure(
                    events_path,
                    pid,
                    "concurrent exec denied while another exec is being inspected",
                );
                frozen.resume.push((pid, 0));
                continue;
            }

            if matches!(
                event as libc::c_ulong,
                PTRACE_EVENT_FORK | PTRACE_EVENT_VFORK | PTRACE_EVENT_CLONE
            ) {
                let new_pid = ptrace_event_pid(pid)?;
                tracees.insert(new_pid);
            }

            // PTRACE_EVENT_STOP/SIGTRAP is the expected interrupt stop.
            // Other stops may be signal deliveries or ptrace events that must
            // be resumed after the protected exec has completed.
            let deliver = if event == PTRACE_EVENT_STOP as i32 && signal == libc::SIGTRAP {
                0
            } else if event == 0 && signal != libc::SIGSTOP && signal != libc::SIGTRAP {
                signal
            } else {
                0
            };
            frozen.resume.push((pid, deliver));
        }
    }

    fn thaw_tracees(
        frozen: &mut FrozenTracees,
        tracees: &mut HashSet<libc::pid_t>,
    ) -> Result<(), SupervisorError> {
        for (pid, signal) in frozen.resume.drain(..) {
            if !tracees.contains(&pid) {
                continue;
            }
            resume_if_alive(pid, signal)?;
        }
        Ok(())
    }

    fn complete_inspected_exec(
        pid: libc::pid_t,
        tracees: &mut HashSet<libc::pid_t>,
        frozen: &mut FrozenTracees,
        deferred_exits: &mut VecDeque<(libc::pid_t, i32)>,
        events_path: &Path,
    ) -> Result<ExecCompletion, SupervisorError> {
        let result = unsafe { libc::ptrace(PTRACE_SYSCALL, pid, 0, 0) };
        if result == -1 {
            return Err(SupervisorError::Ptrace(
                "PTRACE_SYSCALL after exec inspection",
            ));
        }

        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(-1, &mut status, libc::WUNTRACED | libc::__WALL) };
            if waited == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(SupervisorError::Syscall(
                    "waitpid for inspected exec",
                    error,
                ));
            }

            if is_exited(status) || is_signaled(status) {
                tracees.remove(&waited);
                if waited == pid {
                    deferred_exits.push_back((waited, status));
                    return Ok(ExecCompletion::TraceeExited);
                }
                deferred_exits.push_back((waited, status));
                continue;
            }
            if !is_stopped(status) {
                continue;
            }

            let signal = (status >> 8) & 0xff;
            let event = (status >> 16) & 0xffff;
            if event == PTRACE_EVENT_EXEC as i32 {
                let former_tid = ptrace_event_pid(waited)?;
                return Ok(ExecCompletion::Executed {
                    pid: waited,
                    former_tid,
                });
            }
            if waited == pid && event == 0 && signal == SYSCALL_STOP_SIGNAL {
                return Ok(ExecCompletion::SyscallReturned(waited));
            }

            if waited != pid {
                // A tracee that was stopped during the freeze should remain
                // held if an external signal or exit event arrives meanwhile.
                if event == PTRACE_EVENT_SECCOMP as i32 {
                    deny_exec_syscall(waited)?;
                    emit_inspection_failure(
                        events_path,
                        waited,
                        "concurrent exec denied while another exec is being inspected",
                    );
                } else if matches!(
                    event as libc::c_ulong,
                    PTRACE_EVENT_FORK | PTRACE_EVENT_VFORK | PTRACE_EVENT_CLONE
                ) {
                    let child = ptrace_event_pid(waited)?;
                    tracees.insert(child);
                }
                let deliver = if event == 0
                    && signal != libc::SIGSTOP
                    && signal != libc::SIGTRAP
                    && signal != SYSCALL_STOP_SIGNAL
                {
                    signal
                } else {
                    0
                };
                frozen.resume.push((waited, deliver));
                continue;
            }

            // A signal can interrupt the exec attempt. Keep every peer frozen
            // and trace the current thread until it either succeeds or returns.
            let deliver = if event == 0
                && signal != libc::SIGSTOP
                && signal != libc::SIGTRAP
                && signal != SYSCALL_STOP_SIGNAL
            {
                signal
            } else {
                0
            };
            let resumed = unsafe { libc::ptrace(PTRACE_SYSCALL, pid, 0, deliver) };
            if resumed == -1 {
                return Err(SupervisorError::Ptrace(
                    "PTRACE_SYSCALL while awaiting exec result",
                ));
            }
        }
    }

    fn resume_if_alive(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SupervisorError> {
        let result = unsafe { libc::ptrace(PTRACE_CONT, pid, 0, signal) };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(SupervisorError::Syscall("PTRACE_CONT frozen tracee", error));
        }
        Ok(())
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
        let result =
            unsafe { libc::ptrace(PTRACE_GETEVENTMSG, pid, 0, &mut value as *mut libc::c_ulong) };
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

    fn exec_register_arguments(
        registers: &libc::user_regs_struct,
        pid: libc::pid_t,
    ) -> Result<(u64, u64, i64, u64), SupervisorError> {
        if registers.orig_rax == libc::SYS_execve as u64 {
            Ok((registers.rdi, registers.rsi, -1i64, 0u64))
        } else if registers.orig_rax == libc::SYS_execveat as u64 {
            // x86_64 execveat(dirfd, pathname, argv, envp, flags) maps
            // dirfd/pathname/argv/envp/flags to rdi/rsi/rdx/r10/r8. Reading
            // r10 as flags confuses the envp pointer with AT_EMPTY_PATH and
            // can misclassify valid descriptor-based execution.
            Ok((
                registers.rsi,
                registers.rdx,
                registers.rdi as i64,
                registers.r8,
            ))
        } else {
            Err(SupervisorError::InspectFailed(pid))
        }
    }

    fn exec_path_allowed_for_whitelist(path: &str, whitelist_enabled: bool) -> bool {
        !whitelist_enabled || Path::new(path).is_absolute()
    }

    fn inspect_exec(
        pid: libc::pid_t,
        policy: &CommandPolicy,
        events_path: &Path,
    ) -> Result<ExecDecision, SupervisorError> {
        let mut registers = std::mem::MaybeUninit::<libc::user_regs_struct>::uninit();
        let result = unsafe { libc::ptrace(PTRACE_GETREGS, pid, 0, registers.as_mut_ptr()) };
        if result == -1 {
            return Err(SupervisorError::Ptrace("PTRACE_GETREGS"));
        }
        let registers = unsafe { registers.assume_init() };

        let (path_ptr, argv_ptr, dirfd, flags) = exec_register_arguments(&registers, pid)?;

        let mut path = if path_ptr == 0 {
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

        if path.is_empty()
            && registers.orig_rax == libc::SYS_execveat as u64
            && (flags & EXECVEAT_AT_EMPTY_PATH) != 0
        {
            match resolve_execveat_empty_path(pid, dirfd) {
                Some(value) => path = value,
                None => {
                    emit_inspection_failure(
                        events_path,
                        pid,
                        "could not resolve execveat AT_EMPTY_PATH file descriptor",
                    );
                    return Ok(ExecDecision::Deny);
                }
            }
        } else if path.is_empty() {
            path = "<execveat-empty>".to_owned();
        }

        let argv = match read_argv(pid, argv_ptr) {
            Ok(value) => value,
            Err(_) => {
                emit_inspection_failure(events_path, pid, "could not read executable arguments");
                return Ok(ExecDecision::Deny);
            }
        };

        // With an executable whitelist enabled, a relative execve pathname
        // is resolved from the tracee's working directory, not through PATH.
        // A runner can create a same-named binary in that writable directory
        // (for example ./cargo) and otherwise inherit the whitelist by basename.
        // Normal PATH lookup produces an absolute executable path, which the
        // policy separately restricts to the runner image's trusted tool roots.
        if !exec_path_allowed_for_whitelist(&path, policy.user_whitelist.enabled) {
            emit_inspection_failure(
                events_path,
                pid,
                "whitelist mode refuses relative executable paths",
            );
            return Ok(ExecDecision::Deny);
        }

        let command_line = std::iter::once(path.as_str())
            .chain(argv.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");

        match policy.evaluate(&command_line) {
            Decision::Allowed => Ok(ExecDecision::Allow),
            Decision::Denied { reason } => {
                let event = crate::SecurityEvent::new(
                    "gsr-exec-supervisor",
                    crate::Severity::Critical,
                    format!("blocked execve for pid {pid}: {command_line:?} — {reason}"),
                );
                emit_security_event(events_path, &event);
                Ok(ExecDecision::Deny)
            }
        }
    }

    fn deny_exec_syscall(pid: libc::pid_t) -> Result<(), SupervisorError> {
        let mut registers = std::mem::MaybeUninit::<libc::user_regs_struct>::uninit();
        let result = unsafe { libc::ptrace(PTRACE_GETREGS, pid, 0, registers.as_mut_ptr()) };
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
        Ok(())
    }

    fn resolve_execveat_empty_path(pid: libc::pid_t, dirfd: i64) -> Option<String> {
        if dirfd < 0 {
            return None;
        }
        let path = format!("/proc/{pid}/fd/{dirfd}");
        std::fs::read_link(path)
            .ok()
            .and_then(|value| value.into_os_string().into_string().ok())
    }

    fn read_c_string(pid: libc::pid_t, address: u64) -> Result<String, SupervisorError> {
        read_c_string_with(pid, address, |remote, buffer| {
            read_process_memory(pid, remote, buffer)
        })
    }

    fn read_c_string_with<F>(
        pid: libc::pid_t,
        address: u64,
        mut read_memory: F,
    ) -> Result<String, SupervisorError>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, SupervisorError>,
    {
        let mut buffer = vec![0u8; MAX_STRING];
        let bytes = read_memory(address, &mut buffer)?;
        if bytes > buffer.len() {
            return Err(SupervisorError::InspectFailed(pid));
        }
        let Some(end) = buffer[..bytes].iter().position(|byte| *byte == 0) else {
            return Err(SupervisorError::InspectFailed(pid));
        };
        let value =
            std::str::from_utf8(&buffer[..end]).map_err(|_| SupervisorError::InspectFailed(pid))?;
        Ok(value.to_owned())
    }

    fn read_argv(pid: libc::pid_t, address: u64) -> Result<Vec<String>, SupervisorError> {
        read_argv_with(pid, address, |remote, buffer| {
            read_process_memory(pid, remote, buffer)
        })
    }

    fn read_argv_with<F>(
        pid: libc::pid_t,
        address: u64,
        mut read_memory: F,
    ) -> Result<Vec<String>, SupervisorError>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, SupervisorError>,
    {
        // Linux may accept a NULL argv pointer as an empty argument vector,
        // but policy inspection must not silently downgrade to executable-path
        // only. Treat an absent argv table as incomplete inspection and deny it.
        if address == 0 {
            return Err(SupervisorError::InspectFailed(pid));
        }

        const CHUNK: usize = 32;
        const POINTER_SIZE: u64 = std::mem::size_of::<u64>() as u64;
        const CHUNK_BYTES: usize = CHUNK * std::mem::size_of::<u64>();
        let mut args = Vec::new();
        let mut total_bytes = 0usize;

        for chunk_index in 0..(MAX_ARGC / CHUNK) {
            let offset = (chunk_index as u64)
                .checked_mul(CHUNK as u64 * POINTER_SIZE)
                .ok_or(SupervisorError::InspectFailed(pid))?;
            let remote_address = address
                .checked_add(offset)
                .ok_or(SupervisorError::InspectFailed(pid))?;
            let mut raw_pointers = [0u8; CHUNK_BYTES];
            let bytes = read_memory(remote_address, &mut raw_pointers)?;

            if bytes == 0 || bytes > raw_pointers.len() || bytes % POINTER_SIZE as usize != 0 {
                return Err(SupervisorError::InspectFailed(pid));
            }

            for pointer_offset in (0..bytes).step_by(POINTER_SIZE as usize) {
                let pointer_end = pointer_offset
                    .checked_add(POINTER_SIZE as usize)
                    .ok_or(SupervisorError::InspectFailed(pid))?;
                let pointer_bytes: [u8; 8] = raw_pointers[pointer_offset..pointer_end]
                    .try_into()
                    .map_err(|_| SupervisorError::InspectFailed(pid))?;
                let pointer = u64::from_ne_bytes(pointer_bytes);
                if pointer == 0 {
                    return Ok(args);
                }
                let value = read_c_string_with(pid, pointer, &mut read_memory)?;
                total_bytes = total_bytes
                    .checked_add(value.len())
                    .ok_or(SupervisorError::InspectFailed(pid))?;
                if total_bytes > 64 * 1024 {
                    return Err(SupervisorError::InspectFailed(pid));
                }
                args.push(value);
            }

            if bytes < CHUNK_BYTES {
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

    fn kill_tracees(tracees: &HashSet<libc::pid_t>) {
        for &pid in tracees {
            if pid > 0 {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    fn emit_security_event(events_path: &Path, event: &crate::SecurityEvent) {
        if let Err(error) = crate::events::emit(events_path, event) {
            eprintln!(
                "gitrun-gsr-agent: unable to persist critical GSR event to {}: {error}",
                events_path.display()
            );
        }
    }

    fn emit_inspection_failure(events_path: &Path, pid: libc::pid_t, detail: &str) {
        let event = crate::SecurityEvent::new(
            "gsr-exec-supervisor",
            crate::Severity::Critical,
            format!("blocked exec inspection for pid {pid}: {detail}; refusing the exec"),
        );
        emit_security_event(events_path, &event);
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
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;
        use std::sync::atomic::AtomicU64;
        use std::time::Duration;

        // These OS-level tests fork and use ptrace/waitpid. Serialize them so
        // the freezer test cannot reap the seccomp test's unrelated child.
        static OS_PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn test_pid() -> libc::pid_t {
            123
        }

        fn regs(syscall: u64) -> libc::user_regs_struct {
            let mut registers = unsafe { std::mem::zeroed::<libc::user_regs_struct>() };
            registers.orig_rax = syscall;
            registers
        }

        #[test]
        fn seccomp_blocks_untraced_clone_and_makes_clone3_fall_back() {
            let _process_test_guard = OS_PROCESS_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let child = unsafe { libc::fork() };
            assert!(child >= 0, "fork for seccomp filter test failed");
            if child == 0 {
                unsafe {
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                        || install_seccomp_trace_filter() != 0
                    {
                        libc::_exit(101);
                    }

                    let clone_result = libc::syscall(
                        libc::SYS_clone,
                        (u64::from(CLONE_UNTRACED_FLAG) | libc::SIGCHLD as u64) as libc::c_ulong,
                        0usize,
                        0usize,
                        0usize,
                        0usize,
                    );
                    if clone_result == 0 {
                        libc::_exit(102);
                    }
                    if clone_result != -1 || *libc::__errno_location() != libc::EPERM {
                        libc::_exit(103);
                    }

                    // A valid zero-flags clone_args is enough to prove the
                    // filter deliberately returns ENOSYS without creating a
                    // child; unsupported kernels naturally have the same result.
                    let clone_args = [0u64; 11];
                    let clone3_result = libc::syscall(
                        libc::SYS_clone3,
                        clone_args.as_ptr(),
                        std::mem::size_of_val(&clone_args),
                    );
                    if clone3_result == 0 {
                        libc::_exit(104);
                    }
                    if clone3_result != -1 || *libc::__errno_location() != libc::ENOSYS {
                        libc::_exit(105);
                    }

                    // x32 syscall numbers must not fall through the x86-64
                    // execve checks without reaching the tracer.
                    let x32_execve_result = libc::syscall(
                        (X32_SYSCALL_BIT | libc::SYS_execve as u32) as libc::c_long,
                        0usize,
                        0usize,
                        0usize,
                    );
                    if x32_execve_result != -1 || *libc::__errno_location() != libc::ENOSYS {
                        libc::_exit(106);
                    }
                    libc::_exit(0);
                }
            }

            let mut status = 0;
            let wait_result = loop {
                let result = unsafe { libc::waitpid(child, &mut status, 0) };
                if result == child {
                    break result;
                }
                if result == -1
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break result;
            };
            assert_eq!(
                wait_result,
                child,
                "waitpid failed while collecting seccomp test child: {}",
                std::io::Error::last_os_error()
            );
            assert!(
                is_exited(status),
                "filter test subprocess did not exit normally"
            );
            assert_eq!(
                (status >> 8) & 0xff,
                0,
                "seccomp filter did not enforce clone policy"
            );
        }

        #[test]
        fn whitelist_requires_kernel_resolved_absolute_exec_paths() {
            assert!(exec_path_allowed_for_whitelist("/usr/bin/cargo", true));
            assert!(!exec_path_allowed_for_whitelist("./cargo", true));
            assert!(!exec_path_allowed_for_whitelist("cargo", true));
            assert!(!exec_path_allowed_for_whitelist("<execveat-empty>", true));
            assert!(exec_path_allowed_for_whitelist("./cargo", false));
        }

        #[test]
        fn execve_arguments_use_the_x86_64_register_contract() {
            let mut registers = regs(libc::SYS_execve as u64);
            registers.rdi = 0x1111;
            registers.rsi = 0x2222;
            let (path, argv, dirfd, flags) =
                exec_register_arguments(&registers, test_pid()).unwrap();
            assert_eq!((path, argv, dirfd, flags), (0x1111, 0x2222, -1, 0));
        }

        #[test]
        fn execveat_arguments_use_the_x86_64_register_contract() {
            let mut registers = regs(libc::SYS_execveat as u64);
            registers.rdi = (-100i64) as u64;
            registers.rsi = 0x1111;
            registers.rdx = 0x2222;
            registers.r10 = 0x3333; // envp pointer, not flags
            registers.r8 = EXECVEAT_AT_EMPTY_PATH;
            let (path, argv, dirfd, flags) =
                exec_register_arguments(&registers, test_pid()).unwrap();
            assert_eq!(
                (path, argv, dirfd, flags),
                (0x1111, 0x2222, -100, EXECVEAT_AT_EMPTY_PATH)
            );
        }

        #[test]
        fn c_string_reader_rejects_non_utf8_and_missing_terminators() {
            let invalid_utf8 = [0xff, 0x00];
            let decoded = read_c_string_with(test_pid(), 0x10, |_, out| {
                out[..invalid_utf8.len()].copy_from_slice(&invalid_utf8);
                Ok(invalid_utf8.len())
            });
            assert!(decoded.is_err());

            let no_nul = vec![b'a'; MAX_STRING];
            let decoded = read_c_string_with(test_pid(), 0x10, |_, out| {
                out[..no_nul.len()].copy_from_slice(&no_nul);
                Ok(no_nul.len())
            });
            assert!(decoded.is_err());
        }

        #[test]
        fn argv_reader_rejects_null_argument_vector_without_reading_memory() {
            let result = read_argv_with(test_pid(), 0, |_, _| {
                panic!("NULL argv must be rejected before reading tracee memory")
            });
            assert!(
                matches!(result, Err(SupervisorError::InspectFailed(pid)) if pid == test_pid())
            );
        }

        #[test]
        fn argv_reader_rejects_invalid_utf8_truncated_pointers_and_overflow() {
            let mut memory = HashMap::<u64, u8>::new();
            put_bytes(&mut memory, 0x1000, &pointer_table(&[0x2000, 0]));
            put_bytes(&mut memory, 0x2000, &[b'a', 0xff, 0]);
            assert!(read_argv_with(test_pid(), 0x1000, |address, out| {
                read_fake_memory(&memory, address, out)
            })
            .is_err());

            let truncated = HashMap::<u64, u8>::from([(0x1000, 1u8)]);
            assert!(read_argv_with(test_pid(), 0x1000, |address, out| {
                read_fake_memory(&truncated, address, out)
            })
            .is_err());

            let mut empty = HashMap::<u64, u8>::new();
            put_bytes(&mut empty, u64::MAX - 255, &vec![1u8; 256]);
            put_bytes(&mut empty, 0x0101_0101_0101_0101, b"x\0");
            assert!(read_argv_with(test_pid(), u64::MAX - 255, |address, out| {
                read_fake_memory(&empty, address, out)
            })
            .is_err());
        }

        #[test]
        fn argv_reader_rejects_total_argument_bytes_over_limit() {
            let mut memory = HashMap::<u64, u8>::new();
            put_bytes(&mut memory, 0x1000, &pointer_table(&vec![0x2000; 32]));
            let mut long_argument = vec![b'a'; MAX_STRING - 1];
            long_argument.push(0);
            put_bytes(&mut memory, 0x2000, &long_argument);

            assert!(read_argv_with(test_pid(), 0x1000, |address, out| {
                read_fake_memory(&memory, address, out)
            })
            .is_err());
        }

        #[test]
        fn ptrace_resume_and_register_failures_are_reported() {
            assert!(matches!(
                continue_tracee(-1),
                Err(SupervisorError::Ptrace("PTRACE_CONT"))
            ));
            assert!(matches!(
                deny_exec_syscall(-1),
                Err(SupervisorError::Ptrace("PTRACE_GETREGS"))
            ));
        }

        #[test]
        fn argv_reader_rejects_more_than_the_maximum_argument_count() {
            let mut memory = HashMap::<u64, u8>::new();
            let pointer_bytes = pointer_table(&vec![0x2000; MAX_ARGC]);
            put_bytes(&mut memory, 0x1000, &pointer_bytes);
            put_bytes(&mut memory, 0x2000, b"x\0");
            assert!(read_argv_with(test_pid(), 0x1000, |address, out| {
                read_fake_memory(&memory, address, out)
            })
            .is_err());
        }

        // This is an OS-level regression for the primitive that closes the
        // TOCTOU window: a sibling thread shares the same address space and
        // continuously changes a word while the tracer freezes a target thread.
        #[test]
        fn freezer_stops_a_shared_address_space_mutator_until_exec_check_finishes() {
            let _process_test_guard = OS_PROCESS_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            const TEST_STACK_SIZE: usize = 1024 * 1024;

            let counter = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    std::mem::size_of::<AtomicU64>(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(counter, libc::MAP_FAILED);
            let counter = counter.cast::<AtomicU64>();
            unsafe { std::ptr::write(counter, AtomicU64::new(0)) };

            let stack = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    TEST_STACK_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(stack, libc::MAP_FAILED);

            let mut gate = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                unsafe {
                    libc::close(gate[1]);
                    freezer_test_tracee(gate[0], counter, stack.cast(), TEST_STACK_SIZE);
                }
            }
            unsafe { libc::close(gate[0]) };

            let options = ptrace_options();
            let seize = unsafe { libc::ptrace(PTRACE_SEIZE, child, 0, options) };
            if seize == -1 {
                unsafe {
                    libc::kill(child, libc::SIGKILL);
                    libc::close(gate[1]);
                    libc::munmap(counter.cast(), std::mem::size_of::<AtomicU64>());
                    libc::munmap(stack, TEST_STACK_SIZE);
                }
                panic!("could not seize tracee for TOCTOU regression");
            }
            let interrupted = unsafe { libc::ptrace(PTRACE_INTERRUPT, child, 0, 0) };
            if interrupted == -1 || !is_stopped(wait_for_initial_stop(child).unwrap()) {
                unsafe {
                    libc::kill(child, libc::SIGKILL);
                    libc::close(gate[1]);
                    libc::munmap(counter.cast(), std::mem::size_of::<AtomicU64>());
                    libc::munmap(stack, TEST_STACK_SIZE);
                }
                panic!("could not stop tracee during TOCTOU regression setup");
            }

            let ready = [1u8];
            let written = unsafe { libc::write(gate[1], ready.as_ptr().cast(), ready.len()) };
            unsafe { libc::close(gate[1]) };
            assert_eq!(written, 1);
            continue_tracee(child).unwrap();

            let worker = loop {
                let mut status = 0;
                // Wait only for the tracee that reports PTRACE_EVENT_CLONE. A
                // process-wide waitpid(-1) can reap children owned by other
                // concurrently running Rust tests and make their results flaky.
                let waited =
                    unsafe { libc::waitpid(child, &mut status, libc::WUNTRACED | libc::__WALL) };
                assert!(waited > 0, "waiting for test worker clone failed");
                if waited == child
                    && is_stopped(status)
                    && ((status >> 16) & 0xffff) == PTRACE_EVENT_CLONE as i32
                {
                    break ptrace_event_pid(child).unwrap();
                }
                if is_exited(status) || is_signaled(status) {
                    panic!("test tracee exited before creating worker thread");
                }
            };
            continue_tracee(child).unwrap();

            let mut worker_status = 0;
            assert_eq!(
                unsafe {
                    libc::waitpid(worker, &mut worker_status, libc::WUNTRACED | libc::__WALL)
                },
                worker
            );
            assert!(
                is_stopped(worker_status),
                "worker did not enter ptrace stop"
            );
            continue_tracee(worker).unwrap();

            let mut tracees = HashSet::from([child, worker]);
            let mut deferred = VecDeque::new();
            let event_path = std::env::temp_dir().join(format!(
                "gitrun-gsr-freezer-test-{}.jsonl",
                std::process::id()
            ));

            let mut initial = 0;
            for _ in 0..100 {
                initial = unsafe { (*counter).load(Ordering::Relaxed) };
                if initial > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let stop_target = unsafe { libc::ptrace(PTRACE_INTERRUPT, child, 0, 0) };
            assert_ne!(stop_target, -1, "could not stop target tracee");
            assert!(is_stopped(wait_for_initial_stop(child).unwrap()));

            let mut frozen =
                freeze_tracees(child, &mut tracees, &mut deferred, &event_path).unwrap();
            let before = unsafe { (*counter).load(Ordering::Relaxed) };
            std::thread::sleep(Duration::from_millis(25));
            let during = unsafe { (*counter).load(Ordering::Relaxed) };
            let stopped_during_window = before > 0 && during == before;

            thaw_tracees(&mut frozen, &mut tracees).unwrap();
            continue_tracee(child).unwrap();
            std::thread::sleep(Duration::from_millis(25));
            let after = unsafe { (*counter).load(Ordering::Relaxed) };
            let resumed_after_window = after > during;

            unsafe { libc::kill(child, libc::SIGKILL) };
            reap_test_tracees(&mut tracees);
            unsafe {
                libc::munmap(counter.cast(), std::mem::size_of::<AtomicU64>());
                libc::munmap(stack, TEST_STACK_SIZE);
            }

            assert!(initial > 0, "mutator thread never ran");
            assert!(
                stopped_during_window,
                "shared memory changed while peers were frozen"
            );
            assert!(resumed_after_window, "mutator did not resume after thaw");
        }

        extern "C" fn freezer_test_mutator(argument: *mut libc::c_void) -> libc::c_int {
            let counter = argument.cast::<AtomicU64>();
            loop {
                unsafe { (*counter).fetch_add(1, Ordering::Relaxed) };
                unsafe { libc::usleep(100) };
            }
        }

        fn freezer_test_tracee(
            gate_read: RawFd,
            counter: *mut AtomicU64,
            stack: *mut libc::c_void,
            stack_size: usize,
        ) -> ! {
            unsafe {
                if libc::setpgid(0, 0) != 0 {
                    libc::_exit(121);
                }
                let mut ready = 0u8;
                if libc::read(gate_read, (&mut ready as *mut u8).cast(), 1) != 1 || ready != 1 {
                    libc::_exit(122);
                }
                libc::close(gate_read);
                let flags = libc::CLONE_VM
                    | libc::CLONE_FS
                    | libc::CLONE_FILES
                    | libc::CLONE_SIGHAND
                    | libc::CLONE_THREAD;
                let stack_top = stack.cast::<u8>().add(stack_size).cast::<libc::c_void>();
                if libc::clone(
                    freezer_test_mutator,
                    stack_top,
                    flags,
                    counter.cast::<libc::c_void>(),
                ) < 0
                {
                    libc::_exit(123);
                }
                loop {
                    libc::pause();
                }
            }
        }

        fn reap_test_tracees(tracees: &mut HashSet<libc::pid_t>) {
            for _ in 0..500 {
                if tracees.is_empty() {
                    return;
                }
                let pending: Vec<_> = tracees.iter().copied().collect();
                for pid in pending {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    let mut status = 0;
                    let waited =
                        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::__WALL) };
                    if waited == pid {
                        if is_exited(status) || is_signaled(status) {
                            tracees.remove(&pid);
                        } else if is_stopped(status) {
                            let _ = unsafe { libc::ptrace(PTRACE_CONT, pid, 0, libc::SIGKILL) };
                        }
                    }
                }
                if !tracees.is_empty() {
                    unsafe { libc::usleep(1000) };
                }
            }
        }

        fn pointer_table(values: &[u64]) -> Vec<u8> {
            let mut bytes = Vec::with_capacity((values.len() + 1) * 8);
            for value in values {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
            bytes.extend_from_slice(&0u64.to_ne_bytes());
            bytes
        }

        fn put_bytes(memory: &mut HashMap<u64, u8>, start: u64, bytes: &[u8]) {
            for (offset, byte) in bytes.iter().copied().enumerate() {
                memory.insert(start + offset as u64, byte);
            }
        }

        fn read_fake_memory(
            memory: &HashMap<u64, u8>,
            address: u64,
            out: &mut [u8],
        ) -> Result<usize, SupervisorError> {
            let mut count = 0;
            for (offset, slot) in out.iter_mut().enumerate() {
                let Some(value) = memory.get(&address.saturating_add(offset as u64)) else {
                    break;
                };
                *slot = *value;
                count += 1;
            }
            if count == 0 {
                Err(SupervisorError::InspectFailed(test_pid()))
            } else {
                Ok(count)
            }
        }
    }
}

pub fn run_supervisor(events_path: &Path, policy: &CommandPolicy) -> Result<i32, SupervisorError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::run(events_path, policy)
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        run(events_path, policy)
    }
}
