//! Runtime command policy for what may execute inside a runner container.
//!
//! Enforcement model: three independent layers, each with its own explicit
//! switch so deployments can choose the appropriate policy strictness:
//!
//! 1. **Baseline blacklist** (`gsr_command_baseline_blacklist_enabled`,
//!    default **on**) — built-in commands/patterns covering
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
//! an attack surface. Whitelist entries must match the executable and its
//! initial arguments, never an arbitrary later substring, preventing an
//! unrelated executable from being authorized by its arguments. This still
//! trades some false negatives (obfuscated invocations) for a policy that is
//! easy to audit and cannot itself be exploited via crafted input.

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

    /// True if `enabled` and the normalized command contains a pattern at token boundaries.
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
            contains_pattern_at_token_boundaries(command_normalized, &pattern)
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

/// Parse a GitDockRun Execute command into direct argv without invoking a shell.
///
/// The command-policy evaluator is intentionally not a shell parser. This
/// restricted grammar accepts one bare executable name plus plain
/// whitespace-separated arguments, with no shell metacharacters, executable
/// paths, or control characters. Requiring a bare name prevents a whitelist
/// entry such as `cargo` from matching a job-controlled `./cargo` executable
/// in the writable runner directory. The container's executable search path
/// must remain root-owned; the runner image enforces that for toolchain paths.
/// Complex scripts should be checked into the workflow/repository and launched
/// as normal Actions steps, where GSR supervises each actual exec.
pub fn parse_simple_command(command: &str) -> Result<Vec<String>, &'static str> {
    if command.trim().is_empty() {
        return Err("command must not be empty");
    }
    if command.bytes().any(|byte| {
        !(byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b' ' | b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'=' | b',' | b'-'
            ))
    }) {
        return Err(
            "GitDockRun Execute accepts one simple command with plain arguments; shell operators, quoting, expansion, redirection and control characters are not allowed",
        );
    }
    // Execute tokens directly with Docker, not through a shell. The accepted
    // grammar has no quoting, so literal-space splitting is unambiguous and
    // tabs/newlines have already been rejected above.
    let argv: Vec<String> = command
        .split(' ')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if argv.is_empty() {
        return Err("command must not be empty");
    }
    if argv[0].contains('/') || argv[0] == "." || argv[0] == ".." {
        return Err(
            "GitDockRun Execute requires a bare executable name; paths in the executable slot are not allowed",
        );
    }
    Ok(argv)
}

pub fn validate_simple_shell_command(command: &str) -> Result<(), &'static str> {
    parse_simple_command(command).map(|_| ())
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

/// Match a pattern without accepting it as an arbitrary substring of a larger
/// identifier. Punctuation-delimited indicators (URLs, paths and flags) can
/// still match inside a command line; alphanumeric/underscore boundaries
/// prevent cases such as `nsentering` or `ptraceable` from triggering.
fn contains_pattern_at_token_boundaries(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }

    haystack.match_indices(needle).any(|(start, matched)| {
        let end = start + matched.len();
        let starts_with_word = needle
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let ends_with_word = needle
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let left_boundary = !starts_with_word
            || haystack[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let right_boundary = !ends_with_word
            || haystack[end..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        left_boundary && right_boundary
    })
}

/// Canonicalize shell whitespace without attempting to parse shell syntax.
fn normalize_command(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// A whitelist entry must describe the executable and, when present,
/// its initial arguments. It must not match a phrase that appears only in a
/// later argument (for example, a Python script whose filename contains
/// `cargo build --release`).
fn whitelist_executable_name(executable: &str) -> Option<&str> {
    if executable.is_empty() {
        return None;
    }

    // Relative execve paths are resolved from the caller's working directory,
    // not searched through PATH. A workflow can create "./cargo" under its
    // writable workspace; comparing only the basename would turn that file
    // into an allowlisted executable.
    let path = std::path::Path::new(executable);
    if !executable.contains('/') {
        return Some(executable);
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }

    // These are the runner image's root-controlled executable roots. The
    // Dockerfile must keep them non-writable by user runner; repository,
    // workspace, home, temporary and relative paths are not whitelistable by
    // basename. Fully qualified command paths still need to live under a
    // trusted root so a policy cannot bless an arbitrary user-controlled file.
    const TRUSTED_EXECUTABLE_ROOTS: &[&str] = &[
        "/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/sbin",
        "/usr/sbin",
        "/opt/cargo/bin",
        "/opt/rustup/toolchains",
        "/opt/microsoft/powershell",
    ];
    if !TRUSTED_EXECUTABLE_ROOTS
        .iter()
        .any(|root| path.starts_with(root))
    {
        return None;
    }

    path.file_name().and_then(std::ffi::OsStr::to_str)
}

fn whitelist_pattern_matches(pattern: &str, command_normalized: &str) -> bool {
    let pattern = normalize_command(pattern);
    if pattern.is_empty() {
        return false;
    }

    let pattern_tokens = pattern.split(' ').collect::<Vec<_>>();
    let command_tokens = command_normalized.split(' ').collect::<Vec<_>>();

    // A single-token whitelist entry authorizes the executable only.
    // Searching every argument would let an unrelated command smuggle an
    // allowed word later in argv (for example, `python evil.py cargo`).
    // Wrapper commands must be explicitly allowlisted as the executable.
    if pattern_tokens.len() == 1 {
        let Some(executable) = command_tokens.first() else {
            return false;
        };
        return whitelist_executable_name(executable) == Some(pattern_tokens[0]);
    }

    let Some(command_executable) = command_tokens.first() else {
        return false;
    };
    let Some(executable_name) = whitelist_executable_name(command_executable) else {
        return false;
    };
    if executable_name != pattern_tokens[0] {
        return false;
    }

    // The GSR supervisor formats execve as path, argv[0], argv[1..], so
    // argv[0] commonly duplicates the executable. Direct callers/tests may
    // provide the conventional executable, argv[1..] form instead.
    let args_start = if command_tokens
        .get(1)
        .and_then(|token| whitelist_executable_name(token))
        .is_some_and(|token| token == executable_name)
    {
        2
    } else {
        1
    };

    let expected_args = &pattern_tokens[1..];
    let actual_args = command_tokens.get(args_start..).unwrap_or_default();
    actual_args.starts_with(expected_args)
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
    fn simple_shell_command_validator_rejects_compound_commands_and_expansion() {
        for command in [
            "",
            "   ",
            "cargo test; sudo sh",
            "cargo test && curl example.com",
            "cargo test | sh",
            "cargo test > /workspace/out",
            "cargo test $(id)",
            "cargo test \\$(id)",
            "python -c 'import os'",
            "echo `id`",
            "echo $HOME",
            "echo first\necho second",
            "echo first\techo second",
        ] {
            assert!(
                validate_simple_shell_command(command).is_err(),
                "dangerous shell syntax unexpectedly accepted: {command:?}"
            );
        }
    }

    #[test]
    fn simple_command_parser_returns_direct_argv_without_a_shell() {
        assert_eq!(
            parse_simple_command("cargo  build --release").unwrap(),
            vec![
                "cargo".to_owned(),
                "build".to_owned(),
                "--release".to_owned()
            ]
        );
        assert_eq!(
            parse_simple_command("git status --short").unwrap(),
            vec!["git".to_owned(), "status".to_owned(), "--short".to_owned()]
        );
        assert!(parse_simple_command("/usr/bin/git status --short").is_err());
        assert!(parse_simple_command("./cargo build").is_err());
        assert!(parse_simple_command("../bin/cargo build").is_err());
        assert!(parse_simple_command("cargo test; echo injected").is_err());
    }

    #[test]
    fn simple_shell_command_validator_allows_plain_executable_and_arguments() {
        for command in [
            "cargo build --release",
            "git status --short",
            "curl https://example.com/path",
            "docker image inspect gitrun-runner:latest",
            "git -c core.safecrlf=true status",
        ] {
            assert!(
                validate_simple_shell_command(command).is_ok(),
                "ordinary command unexpectedly rejected: {command:?}"
            );
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
    fn blacklist_does_not_match_dangerous_name_inside_unrelated_identifier() {
        let policy = baseline_only();
        assert_eq!(policy.evaluate("echo nsentering"), Decision::Allowed);
        assert_eq!(policy.evaluate("echo ptraceable"), Decision::Allowed);
        assert_eq!(
            policy.evaluate("echo metadata.google.internalish"),
            Decision::Allowed
        );
        assert!(matches!(
            policy.evaluate("curl http://169.254.169.254/latest/meta-data/"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("nsenter --target 1 --mount"),
            Decision::Denied { .. }
        ));
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
    fn whitelist_rejects_user_controlled_executable_paths() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo".into()]),
        };

        // A bare token is resolved through the protected image PATH. An
        // absolute path is accepted only when it sits under a root-controlled
        // executable directory. Paths inside a writable workspace or temp
        // directory cannot inherit an allowlisted basename.
        assert_eq!(policy.evaluate("cargo test"), Decision::Allowed);
        assert_eq!(
            policy.evaluate("/opt/cargo/bin/cargo cargo test"),
            Decision::Allowed
        );
        assert!(matches!(
            policy.evaluate("./cargo cargo test"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("/home/runner/actions-runner/cargo cargo test"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("/tmp/cargo cargo test"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("/opt/cargo/bin/../bin/cargo cargo test"),
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
    fn single_token_whitelist_does_not_match_later_arguments() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo".into()]),
        };

        assert_eq!(policy.evaluate("cargo test"), Decision::Allowed);
        assert_eq!(policy.evaluate("/usr/bin/cargo test"), Decision::Allowed);
        assert!(matches!(
            policy.evaluate("python evil.py cargo"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("echo cargo"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("my-cargo-wrapper build"),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn whitelist_multiword_pattern_matches_executable_and_initial_arguments() {
        let policy = CommandPolicy {
            baseline_blacklist: PatternList::default(),
            user_blacklist: PatternList::default(),
            user_whitelist: PatternList::new(true, vec!["cargo build --release".into()]),
        };

        // Direct command form and the supervisor's path/argv form both match,
        // including additional trailing arguments.
        assert_eq!(
            policy.evaluate("cargo build --release --locked"),
            Decision::Allowed
        );
        assert_eq!(
            policy.evaluate("/usr/bin/cargo cargo build --release --locked"),
            Decision::Allowed
        );
        assert_eq!(
            policy.evaluate("/usr/bin/cargo build --release --locked"),
            Decision::Allowed
        );

        assert!(matches!(
            policy.evaluate("cargo test"),
            Decision::Denied { .. }
        ));
        // The whitelisted phrase appearing later in another executable's
        // arguments must not authorize that executable.
        assert!(matches!(
            policy.evaluate("/usr/bin/python python malicious.py cargo build --release"),
            Decision::Denied { .. }
        ));
        assert!(matches!(
            policy.evaluate("echo cargo build --release"),
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
