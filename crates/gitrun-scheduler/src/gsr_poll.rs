//! GSR's **external** enforcement layer — the safety net for Layer 1
//! (`gitrun_gsr::agent`, the internal shell-wrapper agent installed inside
//! runner containers). See that module's docs for the full two-layer
//! rationale agreed with the operator: internal enforcement is preventive
//! and primary; this layer exists purely to catch what happens if a
//! container's internal agent is bypassed, removed, or never installed in
//! the first place (e.g. a custom runner image that doesn't build on top
//! of GitRun's own).
//!
//! What this does: on each poll tick, lists every GitRun-managed runner
//! container (`docker::all_managed_container_names_on`), reads each one's
//! current process table (`docker::container_command_lines_on`, i.e.
//! `docker top ... -eo args`), and re-evaluates every command line against
//! the same `gitrun_core::CommandPolicy` the internal agent uses. A denied
//! command found here means Layer 1 didn't catch it — by definition, this
//! is either a bypass or a container that never had the agent — so this
//! layer's response is a host-side action Layer 1 usually doesn't need to
//! reach for: killing the container from outside (`docker rm -f`), which
//! works regardless of what's happened inside.
//!
//! # Why polling, not a stream
//! `docker top` gives a point-in-time process list; a short-lived command
//! that starts and exits between two poll ticks can be missed. This is an
//! accepted, documented gap (not a bug to "fix" by polling faster and
//! faster) — the internal agent is the layer that sees every command
//! regardless of duration, and is expected to catch the vast majority of
//! violations before they ever run. This layer's job is to bound how long
//! a *sustained* bypass (a long-running dangerous process, or an attacker
//! who keeps retrying) can go unnoticed, not to guarantee catching
//! everything a stream-based tool would.

use crate::docker::{self, DockerHost};
use gitrun_core::command_policy::{CommandPolicy, Decision, ViolationAction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GsrPollError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid ban state file: {0}")]
    Decode(#[from] serde_json::Error),
}

/// One violation found on a poll tick, for logging/events — deliberately
/// mirrors the shape of `gitrun_gsr::events::SecurityEvent` fields the
/// caller will build from this (kept as its own type here rather than
/// depending on `gitrun-gsr` directly for the event struct, since this
/// module already needs to stay Docker/host-specific and shouldn't also
/// own the event log's wire format).
#[derive(Debug, Clone)]
pub struct PolledViolation {
    pub container_name: String,
    pub repo: Option<String>,
    pub command_line: String,
    pub reason: String,
    pub action_taken: ViolationAction,
}

/// Persisted "banned repos" state: a repo whose runner was killed under
/// `ViolationAction::KillAndBan` is refused new runners until this expires
/// or an operator clears it. Same load/save-atomically pattern as
/// `SchedulerState` (`state.rs`) — a small, single-purpose JSON file in
/// the state directory rather than folding this into that file's more
/// general idle/recovery tracking, since ban entries have a different
/// shape (an expiry timestamp) and a different owner (GSR's policy, not
/// the scheduler's idle-timeout logic).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BanFile {
    /// repo -> unix seconds when the ban expires.
    #[serde(default)]
    banned_until: HashMap<String, u64>,
}

pub struct BanStore {
    path: PathBuf,
    data: BanFile,
}

/// How long a `KillAndBan` ban lasts before it lifts automatically. Not
/// currently operator-configurable (unlike the violation action itself) —
/// kept as a single conservative constant for v1 rather than adding
/// another `Config` field before there's a concrete need for a different
/// duration; an operator can already lift a ban early via `BanStore::clear`
/// (exposed through the dashboard's GSR view) if an hour is wrong for
/// their case.
pub const DEFAULT_BAN_DURATION: Duration = Duration::from_secs(60 * 60);

impl BanStore {
    pub fn load(state_dir: &Path) -> Result<Self, GsrPollError> {
        let path = state_dir.join("gsr-bans.json");
        let data = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BanFile::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, data })
    }

    fn save(&self) -> Result<(), GsrPollError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.data)?)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn ban(&mut self, repo: &str, duration: Duration) -> Result<(), GsrPollError> {
        let until = now_secs() + duration.as_secs();
        self.data.banned_until.insert(repo.to_owned(), until);
        self.save()
    }

    /// Clears a ban early — the operator-facing "unban" action.
    pub fn clear(&mut self, repo: &str) -> Result<(), GsrPollError> {
        self.data.banned_until.remove(repo);
        self.save()
    }

    /// True if `repo` is currently banned. Expired entries are treated as
    /// not-banned but are only actually removed from the file on the next
    /// `ban`/`clear` call that touches this store (lazy cleanup - reading
    /// ban status is on a hot path for "can this repo get a new runner
    /// right now", so it should never itself need a write+fsync).
    pub fn is_banned(&self, repo: &str) -> bool {
        self.data
            .banned_until
            .get(repo)
            .is_some_and(|&until| until > now_secs())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Runs one poll tick: checks every managed runner container's current
/// process table against `policy`, and for each violation found, applies
/// `action` (see `ViolationAction`) — killing the container
/// (`docker rm -f`) for `Kill`/`KillAndBan`, additionally banning the
/// repo for `KillAndBan`, or just returning the finding for `LogOnly`
/// (the caller is responsible for turning returned violations into actual
/// log/event entries either way — this function's job is Docker-side
/// enforcement, not logging, matching how `gitrun_gsr::events` is the
/// single place that owns the event log format).
///
/// A container is only killed once per tick even if multiple of its
/// processes violate policy — there's nothing more to gain from a second
/// `docker rm -f` on the same name.
pub fn poll_once(
    host: &DockerHost,
    policy: &CommandPolicy,
    action: ViolationAction,
    ban_store: &mut BanStore,
) -> docker::Result<Vec<PolledViolation>> {
    let mut violations = Vec::new();
    for container_name in docker::all_managed_container_names_on(host)? {
        let command_lines = docker::container_command_lines_on(host, &container_name)?;
        let mut container_already_handled = false;
        for command_line in command_lines {
            let Decision::Denied { reason } = policy.evaluate(&command_line) else {
                continue;
            };
            if !container_already_handled {
                container_already_handled = true;
                let repo = docker::container_repo_label_on(host, &container_name).unwrap_or(None);
                apply_violation_action(host, &container_name, repo.as_deref(), action, ban_store);
                violations.push(PolledViolation {
                    container_name: container_name.clone(),
                    repo,
                    command_line,
                    reason,
                    action_taken: action,
                });
            }
        }
    }
    Ok(violations)
}

fn apply_violation_action(
    host: &DockerHost,
    container_name: &str,
    repo: Option<&str>,
    action: ViolationAction,
    ban_store: &mut BanStore,
) {
    match action {
        ViolationAction::LogOnly => {
            // Deliberately no Docker action here - see the type's own doc
            // comment on `ViolationAction::LogOnly`: this mode exists so
            // an operator can dry-run a new policy before enforcing it.
        }
        ViolationAction::Kill => {
            let _ = docker::remove_container_on(host, container_name);
        }
        ViolationAction::KillAndBan => {
            let _ = docker::remove_container_on(host, container_name);
            if let Some(repo) = repo {
                if let Err(error) = ban_store.ban(repo, DEFAULT_BAN_DURATION) {
                    eprintln!(
                        "gitrun-scheduler: gsr_poll failed to persist ban for {repo}: {error}"
                    );
                }
            }
        }
    }
}

/// Blocking poll loop, mirroring the shape of `gitrun_gsr::watchdog::run`
/// (poll on an interval until `should_stop` says otherwise) so the two
/// "keep checking on something in a loop" pieces of GSR read the same way
/// even though they watch different things. Intended to run on its own
/// thread inside `gitrun-autoscaler`, started only when
/// `Config::gsr_command_policy_enabled` is true (a caller with the policy
/// disabled should not start this loop at all — `poll_once` still takes an
/// explicit `CommandPolicy` rather than re-checking that flag itself, same
/// division of responsibility as `Config::command_policy()`'s doc comment
/// describes).
pub fn run(
    host: &DockerHost,
    state_dir: &Path,
    policy: CommandPolicy,
    action: ViolationAction,
    poll_interval: Duration,
    on_violation: impl Fn(&PolledViolation),
    should_stop: impl Fn() -> bool,
) {
    let mut ban_store = match BanStore::load(state_dir) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("gitrun-scheduler: gsr_poll could not load ban state, running without ban persistence: {error}");
            BanStore {
                path: state_dir.join("gsr-bans.json"),
                data: BanFile::default(),
            }
        }
    };

    while !should_stop() {
        match poll_once(host, &policy, action, &mut ban_store) {
            Ok(violations) => {
                for violation in &violations {
                    on_violation(violation);
                }
            }
            Err(error) => {
                eprintln!("gitrun-scheduler: gsr_poll tick failed: {error}");
            }
        }
        std::thread::sleep(poll_interval);
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

    fn temp_state_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gitrun-gsr-poll-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ban_store_round_trips_and_expires() {
        let dir = temp_state_dir("roundtrip");
        let mut store = BanStore::load(&dir).unwrap();
        assert!(!store.is_banned("owner/repo"));

        store.ban("owner/repo", Duration::from_secs(3600)).unwrap();
        assert!(store.is_banned("owner/repo"));

        // Reload from disk to confirm persistence, not just in-memory state.
        let reloaded = BanStore::load(&dir).unwrap();
        assert!(reloaded.is_banned("owner/repo"));
        assert!(!reloaded.is_banned("owner/other-repo"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ban_store_immediate_expiry_is_not_banned() {
        let dir = temp_state_dir("expiry");
        let mut store = BanStore::load(&dir).unwrap();
        store.ban("owner/repo", Duration::from_secs(0)).unwrap();
        assert!(!store.is_banned("owner/repo"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ban_store_clear_lifts_ban_early() {
        let dir = temp_state_dir("clear");
        let mut store = BanStore::load(&dir).unwrap();
        store.ban("owner/repo", Duration::from_secs(3600)).unwrap();
        assert!(store.is_banned("owner/repo"));
        store.clear("owner/repo").unwrap();
        assert!(!store.is_banned("owner/repo"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_violation_action_log_only_does_not_touch_ban_store() {
        let dir = temp_state_dir("logonly");
        let mut store = BanStore::load(&dir).unwrap();
        // LogOnly must not call into Docker or touch bans - there's no
        // Docker daemon in this test environment, so if this accidentally
        // tried to shell out it would either hang or error; the absence of
        // a panic/hang here is itself part of what this test checks.
        apply_violation_action(
            &DockerHost::Local,
            "irrelevant",
            Some("owner/repo"),
            ViolationAction::LogOnly,
            &mut store,
        );
        assert!(!store.is_banned("owner/repo"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policy_used_by_poll_matches_agent_policy_semantics() {
        // Sanity check that this module evaluates commands with the exact
        // same CommandPolicy type/semantics the internal agent uses (see
        // gitrun_gsr::agent) - both layers must agree on what's denied,
        // or Layer 2 could kill a container over something Layer 1 was
        // actually right to allow, or vice versa.
        let policy = policy_with_baseline();
        assert!(matches!(
            policy.evaluate("sudo rm -rf /"),
            Decision::Denied { .. }
        ));
        assert_eq!(policy.evaluate("cargo build"), Decision::Allowed);
    }
}
