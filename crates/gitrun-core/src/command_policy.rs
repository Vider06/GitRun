//! Runtime command policy for what may execute inside a runner container.
//!
//! Design (agreed with the operator): three independent layers, each with
//! its own on/off switch, so an operator can mix them freely rather than
//! being forced into one enforcement style:
//!
//! 1. **Baseline blacklist** (`gsr_command_baseline_blacklist_enabled`,
//!    default **on**) — a list of commands/patterns *we* ship, covering
//!    well-known container-escape and credential-theft vectors (`sudo`,
//!    raw `mount`, `ptrace`-capable debuggers, cloud metadata endpoint
//!    access, etc.). This is deliberately not exhaustive — it is a safety
//!    net, not a sandbox — but on by default because these commands have
//!    essentially no legitimate use in a CI job and blocking them costs
//!    nothing for the common case.
//! 2. **User blacklist** (`gsr_command_blacklist_enabled`, default off) —
//!    operator-supplied additional patterns, on top of (or instead of, if
//!    baseline is turned off) our list.
//! 3. **User whitelist** (`gsr_command_whitelist_enabled`, default off) —
//!    if enabled, ONLY commands matching the whitelist are allowed, full
//!    stop; this is the strictest mode and is expected to need real
//!    per-repo tuning, hence off by default.
//!
//! All three can be on simultaneously: a command is allowed only if it
//! passes whitelist (when enabled) AND is not caught by either enabled
//! blacklist. Blacklists use case-insensitive substring matching against a
//! whitespace-normalized command line — intentionally simple (see
//! `CommandPolicy::evaluate`) rather than a shell parser, because a job's
//! command line is already attacker-influenced text and a parser is itself
//! an attack surface. Single-word whitelist entries are additionally matched
//! at command-token boundaries, preventing an entry such as `cargo` from
//! accidentally authorizing `my-cargo-wrapper`; multi-word whitelist entries
//! retain substring matching. This still trades some false negatives
//! (obfuscated invocations) for a policy that is easy to audit and cannot
//! itself be exploited via crafted input.

use serde::{Deserialize, Serialize};

/// One configured policy list: a set of patterns plus whether the list is
/// active at all. Kept as plain `Vec<String>` (not compiled regex) so the
/// list can be shown back to an operator in the dashboard unchanged and so
/// `gitrun-core` doesn't need a regex dependency for something substring
/// matching already covers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatternList {
    pub enabled: bool,
    pub patterns: Vec<String>,
}

impl PatternList {
    pub fn new(enabled: bool, patterns: Vec<String>) -> Self {
        Self { enabled, patterns }
    }

    /// True if `enabled` and the normalized command contains any pattern.
    ///
    /// Whitespace is normalized before matching so equivalent shell spellings
    /// such as `sudo rm`, `sudo\\trm`, and multi-line scripts cannot bypass
    /// a configured pattern merely by changing horizontal/vertical whitespace.
    fn matches(&self, command_normalized: &str) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        self.patterns.iter().map(String::as_str).find(|pattern| {
            let pattern = normalize_command(pattern);
            !pattern.is_empty() && command_normalized.contains(&pattern)
        })
    }
}

/// What GSR should do when `CommandPolicy::evaluate` returns `Denied`.
/// Configured via `Config::gsr_violation_action`; see that field's doc
/// comment for the operator-facing string values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationAction {
    /// Log the event but let the job continue. Useful for dry-running a
    /// new policy before enforcing it.
    LogOnly,
    /// Kill the offending runner container and mark the job as failed.
    /// The default, and the only reasonable choice when a real escape
    /// attempt (not just a policy violation) is suspected.
    Kill,
    /// Kill the container AND temporarily refuse new runners for the same
    /// repo, until an operator clears it. For repeat offenders or a repo
    /// that's actively under attack rather than a one-off bad command.
    KillAndBan,
}

impl ViolationAction {
    pub fn from_config_str(value: &str) -> Self {
        match value {
            "log_only" => Self::LogOnly,
            "kill_and_ban" => Self::KillAndBan,
            _ => Self::Kill,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::LogOnly => "log_only",
            Self::Kill => "kill",
            Self::KillAndBan => "kill_and_ban",
        }
    }
}

/// Result of evaluating one command line against the active policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Carries which list and which pattern caused the denial, for the
    /// security event message (see `gitrun-gsr`) — an operator debugging
    /// "why did my build get killed" needs the exact match, not just
    /// "denied".
    Denied {
        reason: String,
    },
}

/// The full command policy: baseline (ours) + user blacklist + user
/// whitelist, each independently togglable. Construct via
/// `CommandPolicy::from_config` in normal use; the fields are public so
/// tests (and the dashboard, which needs to display current state) can
/// build one directly without going through `Config`.
#[derive(Debug, Clone)]
pub struct CommandPolicy {
    pub baseline_blacklist: PatternList,
    pub user_blacklist: PatternList,
    pub user_whitelist: PatternList,
}

impl CommandPolicy {
    pub fn evaluate(&self, command: &str) -> Decision {
        let normalized = normalize_command(command);

        if let Some(pattern) = self.baseline_blacklist.matches(&normalized) {
            return Decision::Denied {
                reason: format!("matched baseline blacklist pattern {pattern:?}"),
            };
        }
        if let Some(pattern) = self.user_blacklist.matches(&normalized) {
            return Decision::Denied {
                reason: format!("matched user blacklist pattern {pattern:?}"),
            };
        }
        if self.user_whitelist.enabled {
            let allowed = self
                .user_whitelist
                .patterns
                .iter()
                .any(|pattern| whitelist_pattern_matches(pattern, &normalized));
            if !allowed {
                return Decision::Denied {
                    reason: "command not present in whitelist".into(),
                };
            }
        }
        Decision::Allowed
    }
}

/// Canonicalize shell whitespace without attempting to parse shell syntax.
fn normalize_command(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Whitelist entries that contain multiple words keep the documented
/// substring behavior. A single-word whitelist entry, however, must identify
/// an actual command token (or an absolute/relative path ending in that
/// executable name), so `cargo` cannot accidentally authorize
/// `my-cargo-wrapper`.
fn whitelist_pattern_matches(pattern: &str, command_normalized: &str) -> bool {
    let pattern = normalize_command(pattern);
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains(' ') {
        return command_normalized.contains(&pattern);
    }

    command_normalized.split(' ').any(|token| {
        token == pattern
            || token.strip_prefix("./").is_some_and(|p| p == pattern)
            || token.ends_with(&format!("/{pattern}"))
    })
}

/// Our shipped baseline: commands/patterns with essentially no legitimate
/// use inside a CI job container, chosen to cover the vectors most
/// commonly seen in self-hosted-runner compromise write-ups — privilege
/// escalation, raw mount/namespace manipulation, ptrace-based process
/// injection, and reaching cloud provider credential-metadata endpoints
/// (a standard pivot once *any* code execution is achieved on a runner
/// with cloud IAM access). Not a substitute for the Docker-level hardening
/// in `gitrun-gsr::hardening` — this is the second layer, not the only one.
///
/// Deliberately conservative: every entry here either has no reason to
/// appear in a normal build/test/deploy job, or (like `docker run
/// --privileged`) is far more often malicious than not even though the
/// runner does have legitimate Docker access via the mounted socket.
pub fn baseline_patterns() -> Vec<String> {
    [
        // Privilege escalation
        "sudo ",
        "su -",
        "pkexec",
        "doas ",
        // Namespace / mount manipulation (container escape primitives)
        "nsenter",
        "unshare",
        "mount ",
        "mount --bind",
        "chroot ",
        "/proc/1/root",
        "/proc/1/ns/",
        // ptrace-capable process injection / debugging of other processes
        "gdb -p",
        "strace -p",
        "ptrace",
        // Escaping via the mounted Docker socket into a privileged/host-mount container
        "docker run --privileged",
        "docker run -it --privileged",
        "--cap-add=sys_admin",
        "--pid=host",
        "--net=host",
        "-v /:/host",
        "-v /:/mnt/host",
        // Cloud metadata endpoints: the standard credential-theft pivot
        // once code runs on a cloud-hosted runner.
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.azure.com",
        // Kernel module loading
        "insmod ",
        "modprobe ",
        // Rewriting the runner's own registration/audit trail
        "history -c",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_only() -> CommandPolicy {
        CommandPolicy {
            baseline_blacklist: PatternList::new(true, baseline_patterns()),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::default(),
        }
    }

    #[test]
    fn baseline_blocks_sudo() {
        let policy = baseline_only();
        assert!(matches!(
            policy.evaluate("sudo rm -rf /"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn baseline_blocks_metadata_endpoint() {
        let policy = baseline_only();
        assert!(matches!(
            policy.evaluate("curl http://169.254.169.254/latest/meta-data/"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn baseline_allows_ordinary_build_command() {
        let policy = baseline_only();
        assert_eq!(policy.evaluate("cargo build --release"), Decision::Allowed);
    }

    #[test]
    fn disabled_baseline_allows_everything_it_would_otherwise_block() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::new(false, baseline_patterns()),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::default(),
        };
        assert_eq!(policy.evaluate("sudo rm -rf /"), Decision::Allowed);
    }

    #[test]
    fn user_blacklist_is_independent_of_baseline() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::new(false, baseline_patterns()),
            user_blacklist: PatternList::new(true, vec!["rm -rf".into()]),
            user_whitelist: PatternList::default(),
        };
        // baseline off, so sudo alone passes...
        assert_eq!(policy.evaluate("sudo ls"), Decision::Allowed);
        // ...but the user's own pattern still fires regardless.
        assert!(matches!(
            policy.evaluate("rm -rf /tmp/x"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn whitelist_denies_anything_not_listed() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo".into(), "npm".into()]),
        };
        assert_eq!(policy.evaluate("cargo test"), Decision::Allowed);
        assert!(matches!(
            policy.evaluate("python evil.py"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn whitelist_and_blacklist_combine_blacklist_wins() {
        // A command can be in the whitelist's substring net (e.g. "curl" is
        // generically allowed) but still be denied if it also matches an
        // enabled blacklist - blacklist is checked first and always wins,
        // since "allowed in general" must never override "flagged
        // specifically as dangerous".
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::new(true, baseline_patterns()),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["curl".into()]),
        };
        assert!(matches!(
            policy.evaluate("curl http://169.254.169.254/"),
            Decision::Denied { .. }
        ));
        assert_eq!(
            policy.evaluate("curl https://example.com"),
            Decision::Allowed
        );
    }

    #[test]
    fn matching_normalizes_shell_whitespace() {
        let policy = baseline_only();
        assert!(matches!(
            policy.evaluate("sudo\\trm -rf /"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("mount\\n--bind /host /mnt"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn whitelist_single_word_requires_command_boundary() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo".into()]),
        };
        assert_eq!(policy.evaluate("cargo build"), Decision::Allowed);
        assert_eq!(policy.evaluate("/usr/bin/cargo test"), Decision::Allowed);
        assert!(matches!(
            policy.evaluate("my-cargo-wrapper build"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn whitelist_multicharacter_pattern_keeps_substring_behavior() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo build --release".into()]),
        };
        assert_eq!(
            policy.evaluate("cargo build --release --locked"),
            Decision::Allowed
        );
        assert!(matches!(
            policy.evaluate("cargo test"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn violation_action_round_trips_config_strings() {
        assert_eq!(
            ViolationAction::from_config_str("log_only"),
            ViolationAction::LogOnly
        );
        assert_eq!(
            ViolationAction::from_config_str("kill_and_ban"),
            ViolationAction::KillAndBan
        );
        assert_eq!(
            ViolationAction::from_config_str("kill"),
            ViolationAction::Kill
        );
        assert_eq!(
            ViolationAction::from_config_str("anything-else"),
            ViolationAction::Kill
        );
        for action in [
            ViolationAction::LogOnly,
            ViolationAction::Kill,
            ViolationAction::KillAndBan,
        ] {
            assert_eq!(
                ViolationAction::from_config_str(action.as_config_str()),
                action
            );
        }
    }
}
