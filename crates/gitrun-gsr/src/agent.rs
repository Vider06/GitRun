//! GSR's internal enforcement layers inside Linux runner containers.
//!
//! Layer 1 is the shell replacement: gitrun-gsr-agent is installed as
//! /bin/sh and /bin/bash and rejects denied run-step command lines before
//! the real shell starts.
//!
//! The kernel supervisor in exec_supervisor.rs sits underneath that layer.
//! It owns PID 1 in the runner container, launches the Actions runner as the
//! unprivileged runner account, and receives a ptrace stop for every
//! execve/execveat performed by the runner or one of its descendants. This
//! catches direct execve calls from Python/Node/compiled helpers and removes
//! the timing gap in which a short-lived process could evade docker top
//! polling.
//!
//! The real shell binaries are still relocated to fixed paths. That is useful
//! for the shell layer, while the kernel layer guarantees that directly
//! invoking those paths does not bypass the policy.

use crate::events::{self, SecurityEvent, Severity};
use gitrun_core::command_policy::{CommandPolicy, Decision};
use std::env;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;

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

/// Creates the effective policy used by both the shell layer and the kernel
/// supervisor. A disabled master switch intentionally becomes an empty
/// policy rather than a special execution path, so the two layers keep the
/// same semantics.
fn policy_for_config(config: &gitrun_core::Config) -> CommandPolicy {
    config.command_policy().unwrap_or_else(|| CommandPolicy {
        baseline_blacklist: gitrun_core::PatternList::default(),
        user_blacklist: gitrun_core::PatternList::default(),
        user_whitelist: gitrun_core::PatternList::default(),
    })
}

/// Extracts the command represented by a shell invocation, from sh -c
/// script-style argv when present. For direct script-file invocations it
/// falls back to joining argv so a policy still has something concrete to
/// inspect.
pub fn extract_command_line(args: &[String]) -> String {
    if let Some(dash_c_pos) = args.iter().position(|a| a == "-c") {
        if let Some(script) = args.get(dash_c_pos + 1) {
            return script.clone();
        }
    }
    args.join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDecision {
    Delegate { real_shell: String },
    Refuse { reason: String },
}

/// Pure decision logic for one shell invocation. It does not read mutable
/// policy state from the process environment, which keeps the decision easy
/// to test and prevents a job from changing policy after startup.
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

/// Executes the shell-wrapper layer directly. The kernel supervisor uses the
/// same policy builder but does not call this function; it supervises the
/// whole Actions runner process tree instead.
pub fn run(events_path: &std::path::Path, config: &gitrun_core::Config) -> i32 {
    let args: Vec<String> = env::args().skip(1).collect();
    let policy = policy_for_config(config);
    let invocation = env::args().next().unwrap_or_else(|| "sh".to_owned());

    match decide(&policy, &args, &invocation) {
        AgentDecision::Delegate { real_shell } => {
            #[cfg(unix)]
            {
                let error = Command::new(&real_shell).args(&args).exec();
                eprintln!("gitrun-gsr-agent: failed to exec real shell {real_shell:?}: {error}");
                127
            }
            #[cfg(not(unix))]
            {
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
            let event = SecurityEvent::new("gsr-agent", Severity::Warning, reason);
            if let Err(write_error) = events::emit(events_path, &event) {
                eprintln!(
                    "gitrun-gsr-agent: additionally failed to write security event: {write_error}"
                );
            }
            1
        }
    }
}

/// Starts the kernel-backed supervisor. This function intentionally fails
/// closed if ptrace/seccomp setup or executable inspection is unavailable:
/// falling back to the weaker shell layer would recreate the bypass this
/// supervisor is specifically meant to close.
pub fn supervise_runner(events_path: &std::path::Path, config: &gitrun_core::Config) -> i32 {
    let policy = policy_for_config(config);
    match crate::exec_supervisor::run_supervisor(events_path, &policy) {
        Ok(code) => code,
        Err(error) => {
            let message = format!("GSR exec supervisor failed closed: {error}");
            eprintln!("gitrun-gsr-agent: {message}");
            let event =
                SecurityEvent::new("gsr-exec-supervisor", Severity::Critical, message.clone());
            if let Err(write_error) = events::emit(events_path, &event) {
                eprintln!(
                    "gitrun-gsr-agent: additionally failed to write supervisor event: {write_error}"
                );
            }
            125
        }
    }
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
    fn policy_disabled_is_empty_but_still_shared() {
        let config = gitrun_core::Config {
            gsr_command_policy_enabled: false,
            ..gitrun_core::Config::default()
        };
        assert_eq!(
            policy_for_config(&config).evaluate("sudo rm -rf /"),
            Decision::Allowed
        );
    }
}
