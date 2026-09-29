//! GSR's **internal** enforcement layer: `gitrun-gsr-agent`, a tiny binary
//! installed as `/bin/sh` (and the `/bin/bash` symlink) inside runner
//! container images, replacing the real shell. This is Layer 1 of the
//! two-layer design agreed with the operator (preventive, inside the
//! container) — see `gitrun-scheduler`'s external `docker top` poller for
//! Layer 2 (the safety net on the host, in case this layer is bypassed or
//! removed from a compromised container).
//!
//! # Why replace the shell rather than something else
//! The official GitHub Actions runner always executes a workflow step's
//! `run:` block via a shell — `sh -c "<script>"` by default, or `bash -c`
//! when the step sets `shell: bash` (bash is usually a symlink to the same
//! binary as `sh`, or a near-identical build, on the images GitRun's
//! runners are based on). Replacing that one binary is the single
//! narrowest point that still sees literally every step's command line,
//! without patching the runner binary itself (which GitHub updates
//! independently of GitRun and which `gitrun-updater` already handles
//! updating - re-patching it here would fight that) and without needing a
//! kernel-level mechanism (ptrace/seccomp-notify) that would need root
//! inside the container and a lot more surface to get right for a v1.
//!
//! # What this does and does not catch
//! Every `run:` step goes through this, because that's how the runner
//! invokes user commands. What it does NOT see: a compiled program a step
//! runs that itself forks/execs further children directly (bypassing
//! `/bin/sh` entirely) — e.g. a Python script using `subprocess.run` calls
//! the target binary via `execve`, not through this shell, so those child
//! commands are invisible to this layer. That gap is exactly why Layer 2
//! (external `docker top` polling, see `gitrun_scheduler::gsr_poll`)
//! exists: it sees the resulting process table from outside, catching
//! what this layer's shell-only vantage point misses.
//!
//! # Real shell delegation
//! On `Decision::Allowed`, this execs one of the fixed real shell binaries
//! relocated by the runner image at build time, using the same arguments, so
//! allowed commands behave completely normally; this binary is invisible to
//! a passing step. On `Decision::Denied`, it refuses to exec anything,
//! writes a security event, and exits non-zero so the step (and therefore the
//! job) fails the same way a normal shell syntax error would - the person
//! reviewing the failed job sees a clear message either way.

use crate::events::{self, SecurityEvent, Severity};
use gitrun_core::command_policy::{CommandPolicy, Decision};
use std::env;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Fixed paths to the real shell binaries relocated at image-build time.
/// These are deliberately constants rather than environment-controlled
/// paths, because the command-policy boundary must not be bypassable by
/// changing the delegation target.
const REAL_SHELL_FALLBACK: &str = "/bin/sh.gitrun-real";
const REAL_BASH_FALLBACK: &str = "/bin/bash.gitrun-real";

fn is_bash_invocation(invocation: &str) -> bool {
    invocation.ends_with("/bash") || invocation == "bash"
}

fn real_shell_for_invocation(invocation: &str) -> &'static str {
    if is_bash_invocation(invocation) {
        REAL_BASH_FALLBACK
    } else {
        REAL_SHELL_FALLBACK
    }
}

/// Extracts the full command line this invocation represents, from `sh -c
/// "<script>"`-style argv. GitHub Actions always invokes the step shell as
/// `sh -c <script> [args...]` (with the script as a single argv element,
/// not already split into words) - see the runner's `run.sh` template
/// generation - so `args[2]` (after the program name and `-c`) is the
/// text we evaluate. Falls back to joining all arguments if the shape
/// doesn't match `-c`, since some shells are invoked directly with a
/// script file path (`sh /path/to/script`) rather than `-c`; either way
/// we want *some* representation of what's about to run to check.
pub fn extract_command_line(args: &[String]) -> String {
    if let Some(dash_c_pos) = args.iter().position(|a| a == "-c") {
        if let Some(script) = args.get(dash_c_pos + 1) {
            return script.clone();
        }
    }
    args.join(" ")
}

/// Outcome of evaluating one invocation, separated from the process exit
/// itself so it's testable without actually exec'ing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDecision {
    /// Exec the real shell with these exact arguments.
    Delegate { real_shell: String },
    /// Refuse to run; `reason` is both logged and printed to stderr so the
    /// job log shows why the step failed.
    Refuse { reason: String },
}

/// Pure decision logic, taking the policy and raw argv (excluding `argv[0]`,
/// i.e. what the shell was invoked with) rather than reading the
/// environment/process directly, so it's fully unit-testable.
pub fn decide(policy: &CommandPolicy, args: &[String], invocation: &str) -> AgentDecision {
    let command_line = extract_command_line(args);
    match policy.evaluate(&command_line) {
        Decision::Allowed => AgentDecision::Delegate {
            real_shell: real_shell_for_invocation(invocation).to_owned(),
        },
        Decision::Denied { reason } => AgentDecision::Refuse {
            reason: format!(
                "gitrun-gsr-agent blocked this command: {reason} (command: {command_line:?})"
            ),
        },
    }
}

/// Runs the agent's full logic against the real process environment and
/// argv, and never returns on the `Delegate` path (it `exec`s in place,
/// replacing this process exactly like a real shell would - so the job
/// step sees no difference in PID, signal handling, etc.). Returns an exit
/// code only on the `Refuse` path (or if `exec` itself fails, which means
/// the image is misconfigured - the real shell binary is missing).
pub fn run(events_path: &std::path::Path, config: &gitrun_core::Config) -> i32 {
    let args: Vec<String> = env::args().skip(1).collect();
    let policy = match config.command_policy() {
        Some(policy) => policy,
        // Master switch off: every command is allowed, unconditionally -
        // still delegate through decide() for a single code path rather
        // than special-casing "policy disabled" separately here.
        None => CommandPolicy {
            baseline_blacklist: gitrun_core::PatternList::default(),
            user_blacklist: gitrun_core::PatternList::default(),
            user_whitelist: gitrun_core::PatternList::default(),
        },
    };
    let invocation = env::args().next().unwrap_or_else(|| "sh".to_owned());
    match decide(&policy, &args, &invocation) {
        AgentDecision::Delegate { real_shell } => {
            // exec replaces this process; on success this call never
            // returns. On failure (real shell missing/not executable),
            // fall through to report that as a misconfiguration rather
            // than silently doing nothing.
            #[cfg(unix)]
            {
                let error = Command::new(&real_shell).args(&args).exec();
                eprintln!("gitrun-gsr-agent: failed to exec real shell {real_shell:?}: {error}");
                127
            }
            #[cfg(not(unix))]
            {
                // The agent is Linux-only for now (matches GSR's overall
                // v1 scope - Windows runner container support is planned
                // separately, see the project's packaging notes). Spawn +
                // wait rather than a true exec, since Windows has no
                // process-replacement primitive exposed the same way;
                // this is here so the crate at least builds on Windows,
                // not as a production path yet.
                match Command::new(&real_shell).args(&args).status() {
                    Ok(status) => status.code().unwrap_or(1),
                    Err(error) => {
                        eprintln!(
                            "gitrun-gsr-agent: failed to run real shell {real_shell:?}: {error}"
                        );
                        127
                    }
                }
            }
        }
        AgentDecision::Refuse { reason } => {
            eprintln!("{reason}");
            let event = SecurityEvent::new("gsr-agent", severity_for(config), reason);
            if let Err(write_error) = events::emit(events_path, &event) {
                eprintln!(
                    "gitrun-gsr-agent: additionally failed to write security event: {write_error}"
                );
            }
            1
        }
    }
}

/// Command-policy violations inside a job are always at least a Warning
/// (someone's build tried to do something explicitly disallowed) - never
/// silently Info, since a false-positive policy match is something an
/// operator needs to see and possibly tune, not something to bury.
fn severity_for(_config: &gitrun_core::Config) -> Severity {
    Severity::Warning
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitrun_core::{baseline_patterns, PatternList};

    fn policy_with_baseline() -> CommandPolicy {
        CommandPolicy {
            baseline_blacklist: PatternList::new(true, baseline_patterns()),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::default(),
        }
    }

    #[test]
    fn extract_command_line_handles_dash_c_form() {
        let args = vec!["-c".to_string(), "cargo build --release".to_string()];
        assert_eq!(extract_command_line(&args), "cargo build --release");
    }

    #[test]
    fn extract_command_line_falls_back_to_joining_args() {
        let args = vec!["/path/to/script.sh".to_string()];
        assert_eq!(extract_command_line(&args), "/path/to/script.sh");
    }

    #[test]
    fn allowed_command_delegates_to_real_shell() {
        let policy = policy_with_baseline();
        let args = vec!["-c".to_string(), "cargo test".to_string()];
        let decision = decide(&policy, &args, "sh");
        assert_eq!(
            decision,
            AgentDecision::Delegate {
                real_shell: "/bin/sh.gitrun-real".into()
            }
        );
    }

    #[test]
    fn denied_command_refuses_with_reason_including_command_text() {
        let policy = policy_with_baseline();
        let args = vec!["-c".to_string(), "sudo rm -rf /".to_string()];
        match decide(&policy, &args, "sh") {
            AgentDecision::Refuse { reason } => {
                assert!(reason.contains("sudo rm -rf /"));
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn bash_invocation_uses_bash_real_shell() {
        assert_eq!(real_shell_for_invocation("bash"), REAL_BASH_FALLBACK);
        assert_eq!(real_shell_for_invocation("/bin/bash"), REAL_BASH_FALLBACK);
    }

    #[test]
    fn sh_invocation_uses_sh_real_shell() {
        assert_eq!(real_shell_for_invocation("sh"), REAL_SHELL_FALLBACK);
        assert_eq!(real_shell_for_invocation("/bin/sh"), REAL_SHELL_FALLBACK);
    }

    #[test]
    fn unknown_invocation_uses_sh_real_shell() {
        assert_eq!(real_shell_for_invocation("dash"), REAL_SHELL_FALLBACK);
    }

    #[test]
    fn missing_real_shell_env_uses_fixed_path() {
        let policy = policy_with_baseline();
        let args = vec!["-c".to_string(), "echo hi".to_string()];
        let decision = decide(&policy, &args, "sh");
        assert_eq!(
            decision,
            AgentDecision::Delegate {
                real_shell: REAL_SHELL_FALLBACK.into()
            }
        );
    }
}
