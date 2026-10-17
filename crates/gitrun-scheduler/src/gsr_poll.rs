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
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GsrPollError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid ban state file: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unsafe GSR ban state path: {0}")]
    UnsafePath(String),
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
    /// Redacted before being exposed to the event/logging layer.
    /// The full command line is still evaluated by the policy.
    pub command_line: String,
    pub reason: String,
    pub action_taken: ViolationAction,
    /// Present when the configured enforcement action was only partially
    /// applied or could not be applied.
    pub action_error: Option<String>,
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
        match fs::symlink_metadata(state_dir) {
            Ok(_) => validate_ban_state_directory(state_dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    data: BanFile::default(),
                });
            }
            Err(error) => return Err(error.into()),
        }
        let data = read_ban_file(&path)?.unwrap_or_default();
        Ok(Self { path, data })
    }

    /// Creates a store handle when persisted ban state cannot currently be
    /// read. Polling can still enforce Kill/LogOnly, and KillAndBan still
    /// attempts to terminate a violating container; persistence errors are
    /// surfaced by ban() rather than disabling all external enforcement.
    fn unavailable(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("gsr-bans.json"),
            data: BanFile::default(),
        }
    }

    fn lock(&self) -> Result<File, GsrPollError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        validate_ban_state_directory(parent)?;
        let lock_path = self.path.with_extension("json.lock");
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let lock = options.open(&lock_path)?;
        validate_ban_file_metadata(&lock.metadata()?, &lock_path)?;
        lock.lock()?;
        Ok(lock)
    }

    fn reload_from_disk(&mut self) -> Result<(), GsrPollError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        validate_ban_state_directory(parent)?;
        self.data = read_ban_file(&self.path)?.unwrap_or_default();
        Ok(())
    }

    fn save(&self) -> Result<(), GsrPollError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let serialized = serde_json::to_vec_pretty(&self.data)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp = parent.join(format!(".gsr-bans.{}.{}.tmp", std::process::id(), nonce));

        // create_new avoids following a pre-created symlink and keeps
        // concurrent saves from truncating one another's temporary file.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        if let Err(error) =
            std::io::Write::write_all(&mut file, &serialized).and_then(|()| file.sync_all())
        {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        if let Err(error) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn ban(&mut self, repo: &str, duration: Duration) -> Result<(), GsrPollError> {
        let _lock = self.lock()?;
        self.reload_from_disk()?;
        let until = now_secs().saturating_add(duration.as_secs());
        self.data.banned_until.insert(repo.to_owned(), until);
        self.save()
    }

    /// Clears a ban early — the operator-facing "unban" action.
    pub fn clear(&mut self, repo: &str) -> Result<(), GsrPollError> {
        let _lock = self.lock()?;
        self.reload_from_disk()?;
        if self.data.banned_until.remove(repo).is_none() {
            return Ok(());
        }
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

fn validate_ban_state_directory(path: &Path) -> Result<(), GsrPollError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GsrPollError::UnsafePath(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode();
        if mode & 0o022 != 0 && !(mode & 0o1000 != 0) {
            return Err(GsrPollError::UnsafePath(format!(
                "{} is group/world-writable without the sticky bit",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_ban_file_metadata(metadata: &fs::Metadata, path: &Path) -> Result<(), GsrPollError> {
    if !metadata.is_file() {
        return Err(GsrPollError::UnsafePath(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
            return Err(GsrPollError::UnsafePath(format!(
                "{} must have one link and no group/world write permission",
                path.display()
            )));
        }
    }
    Ok(())
}

fn read_ban_file(path: &Path) -> Result<Option<BanFile>, GsrPollError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_ban_file_metadata(&file.metadata()?, path)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    Ok(Some(serde_json::from_str(&raw)?))
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
        // Pin the Docker object identity before inspecting its processes. Every
        // subsequent read and enforcement action uses this ID, never the
        // mutable name: a container recreated under the same name between
        // inspection and enforcement must not be killed as the original.
        let container_id = match docker::container_id_on(host, &container_name) {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(error) => {
                eprintln!(
                    "gitrun-scheduler: gsr_poll could not pin Docker identity for {container_name} on {host:?}; skipping this container for this tick: {error}"
                );
                continue;
            }
        };

        violations.extend(poll_container_with_identity(
            host,
            &container_name,
            &container_id,
            policy,
            action,
            ban_store,
            PollOperations {
                read_processes: |target: &str| docker::container_command_lines_on(host, target),
                read_repo: |target: &str| docker::container_repo_label_on(host, target),
                enforce: |target: &str,
                          repo: Option<&str>,
                          action: ViolationAction,
                          store: &mut BanStore| {
                    apply_violation_action(host, target, repo, action, store)
                },
            },
        ));
    }
    Ok(violations)
}

/// Operations are injected only to make the identity flow deterministic in
/// regression tests; production supplies the real Docker inspection and
/// enforcement calls above.
struct PollOperations<P, R, A> {
    read_processes: P,
    read_repo: R,
    enforce: A,
}

/// Process one container using a pinned immutable Docker ID. Keeping the
/// callbacks together leaves identity and policy parameters explicit without
/// a long positional-argument list.
fn poll_container_with_identity<P, R, A, ProcessError, RepoError>(
    host: &DockerHost,
    container_name: &str,
    container_id: &str,
    policy: &CommandPolicy,
    action: ViolationAction,
    ban_store: &mut BanStore,
    operations: PollOperations<P, R, A>,
) -> Vec<PolledViolation>
where
    P: FnMut(&str) -> Result<Vec<String>, ProcessError>,
    ProcessError: std::fmt::Display,
    R: FnMut(&str) -> Result<Option<String>, RepoError>,
    RepoError: std::fmt::Display,
    A: FnMut(&str, Option<&str>, ViolationAction, &mut BanStore) -> Option<String>,
{
    let PollOperations {
        mut read_processes,
        mut read_repo,
        mut enforce,
    } = operations;
    let command_lines = match read_processes(container_id) {
        Ok(command_lines) => command_lines,
        Err(error) => {
            eprintln!(
                "gitrun-scheduler: gsr_poll could not inspect {container_name} ({container_id}) on {host:?}: {error}"
            );
            return Vec::new();
        }
    };

    let mut violations = Vec::new();
    let mut container_already_handled = false;
    for command_line in command_lines {
        let Decision::Denied { reason } = policy.evaluate(&command_line) else {
            continue;
        };
        if container_already_handled {
            continue;
        }
        container_already_handled = true;

        // Query the repo label and enforce against the same object whose
        // process list produced this violation.
        let (repo, repo_error) = match read_repo(container_id) {
            Ok(repo) => (repo, None),
            Err(error) => {
                let detail = format!(
                    "unable to inspect repo label for {container_name} ({container_id}): {error}"
                );
                eprintln!("gitrun-scheduler: gsr_poll: {detail}");
                (None, Some(detail))
            }
        };
        let action_error = enforce(container_id, repo.as_deref(), action, ban_store);
        let action_error = match (repo_error, action_error) {
            (Some(repo_error), Some(action_error)) => Some(format!("{repo_error}; {action_error}")),
            (Some(error), None) | (None, Some(error)) => Some(error),
            (None, None) => None,
        };
        violations.push(PolledViolation {
            container_name: container_name.to_owned(),
            repo,
            command_line: redact_command_line(&command_line),
            reason,
            action_taken: action,
            action_error,
        });
    }

    violations
}

fn apply_violation_action(
    host: &DockerHost,
    container_id: &str,
    repo: Option<&str>,
    action: ViolationAction,
    ban_store: &mut BanStore,
) -> Option<String> {
    match action {
        ViolationAction::LogOnly => None,
        ViolationAction::Kill => docker::remove_container_on(host, container_id)
            .err()
            .map(|error| format!("container kill failed for pinned ID {container_id}: {error}")),
        ViolationAction::KillAndBan => {
            // Persist the repo ban first: if container removal subsequently
            // fails, the scheduler still refuses to create a replacement runner.
            let ban_error = match repo {
                Some(repo) => ban_store
                    .ban(repo, DEFAULT_BAN_DURATION)
                    .err()
                    .map(|error| format!("repository ban persistence failed: {error}")),
                None => Some(
                    "repository label was unavailable; repository ban was not applied".to_owned(),
                ),
            };
            let kill_error = docker::remove_container_on(host, container_id)
                .err()
                .map(|error| {
                    format!("container kill failed for pinned ID {container_id}: {error}")
                });

            match (ban_error, kill_error) {
                (Some(ban), Some(kill)) => Some(format!("{ban}; {kill}")),
                (Some(error), None) | (None, Some(error)) => Some(error),
                (None, None) => None,
            }
        }
    }
}

fn redact_command_line(command_line: &str) -> String {
    let mut parts = command_line.split_whitespace();
    let Some(program) = parts.next() else {
        return "<empty command>".to_owned();
    };
    if parts.next().is_some() {
        format!("{program} [arguments redacted]")
    } else {
        program.to_owned()
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
    run_with_hosts(
        state_dir,
        policy,
        action,
        poll_interval,
        move || vec![host.clone()],
        on_violation,
        should_stop,
    );
}

/// Polls a dynamically supplied set of Docker daemons each tick. This keeps
/// GSR enforcement active for both the local daemon and Logic-Containers VM
/// daemons that become available after the poll thread has started.
pub fn run_with_hosts(
    state_dir: &Path,
    policy: CommandPolicy,
    action: ViolationAction,
    poll_interval: Duration,
    host_provider: impl Fn() -> Vec<DockerHost>,
    on_violation: impl Fn(&PolledViolation),
    should_stop: impl Fn() -> bool,
) {
    if poll_interval.is_zero() {
        eprintln!(
            "gitrun-scheduler: gsr_poll received a zero poll interval; refusing to busy-loop"
        );
        return;
    }

    let mut ban_store = match BanStore::load(state_dir) {
        Ok(store) => store,
        Err(error) => {
            // Do not disable all host-side process enforcement because the
            // optional ban state is unavailable. Kill/LogOnly remain useful,
            // and KillAndBan still kills the container while reporting any
            // persistence failure; the next ban attempt re-reads the file.
            eprintln!(
                "gitrun-scheduler: gsr_poll could not load ban state; continuing enforcement with persistence errors surfaced: {error}"
            );
            BanStore::unavailable(state_dir)
        }
    };

    while !should_stop() {
        for host in host_provider() {
            match poll_once(&host, &policy, action, &mut ban_store) {
                Ok(violations) => {
                    for violation in &violations {
                        on_violation(violation);
                    }
                }
                Err(error) => {
                    eprintln!("gitrun-scheduler: gsr_poll tick failed on {host:?}: {error}");
                }
            }
        }
        sleep_interruptible(poll_interval, &should_stop);
    }
}

fn sleep_interruptible(duration: Duration, should_stop: &impl Fn() -> bool) {
    let deadline = Instant::now().checked_add(duration);
    while !should_stop() {
        let Some(deadline) = deadline else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
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
    fn ban_store_first_run_accepts_missing_state_directory() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-gsr-poll-first-run-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut store = BanStore::load(&dir).unwrap();
        assert!(!store.is_banned("owner/repo"));
        store.ban("owner/repo", Duration::from_secs(60)).unwrap();
        assert!(store.is_banned("owner/repo"));
        let _ = fs::remove_dir_all(&dir);
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

    #[cfg(unix)]
    #[test]
    fn ban_store_lock_rejects_symlink_without_modifying_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_state_dir("lock-symlink");
        let target = dir.join("must-not-be-touched");
        fs::write(&target, "preserve me").unwrap();
        symlink(&target, dir.join("gsr-bans.json.lock")).unwrap();

        let mut store = BanStore::load(&dir).unwrap();
        assert!(store.ban("owner/repo", Duration::from_secs(3600)).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "preserve me");
        assert!(!dir.join("gsr-bans.json").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ban_store_load_rejects_symlink_without_reading_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_state_dir("state-symlink");
        let target = dir.join("external-state.json");
        fs::write(&target, r#"{"banned_until":{}}"#).unwrap();
        symlink(&target, dir.join("gsr-bans.json")).unwrap();

        assert!(BanStore::load(&dir).is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"banned_until":{}}"#
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ban_store_load_rejects_hard_linked_state() {
        let dir = temp_state_dir("state-hardlink");
        let target = dir.join("external-state.json");
        let state = dir.join("gsr-bans.json");
        fs::write(&target, r#"{"banned_until":{}}"#).unwrap();
        fs::hard_link(&target, &state).unwrap();

        assert!(BanStore::load(&dir).is_err());
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
        assert_eq!(
            apply_violation_action(
                &DockerHost::Local,
                "irrelevant",
                Some("owner/repo"),
                ViolationAction::LogOnly,
                &mut store,
            ),
            None
        );
        assert!(!store.is_banned("owner/repo"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gsr_poll_pins_process_label_and_enforcement_operations_to_one_container_id() {
        use std::cell::RefCell;

        let dir = temp_state_dir("pinned-container-id");
        let mut ban_store = BanStore::load(&dir).unwrap();
        let name = "gitrun-runner-reused";
        let original_id = "test-container-id-runner-a";
        let replacement_id = "test-container-id-replacement-b";
        let targets = RefCell::new(Vec::new());

        let violations = poll_container_with_identity(
            &DockerHost::Local,
            name,
            original_id,
            &policy_with_baseline(),
            ViolationAction::Kill,
            &mut ban_store,
            PollOperations {
                read_processes: |target: &str| {
                    assert_eq!(target, original_id);
                    assert_ne!(target, replacement_id);
                    targets.borrow_mut().push(("processes", target.to_owned()));
                    Ok::<_, &'static str>(vec!["sudo rm -rf /".to_owned()])
                },
                read_repo: |target: &str| {
                    assert_eq!(target, original_id);
                    assert_ne!(target, replacement_id);
                    targets.borrow_mut().push(("repo-label", target.to_owned()));
                    Ok::<_, &'static str>(Some("owner/repo".to_owned()))
                },
                enforce: |target: &str,
                          repo: Option<&str>,
                          action: ViolationAction,
                          _store: &mut BanStore| {
                    assert_eq!(target, original_id);
                    assert_ne!(target, replacement_id);
                    assert_eq!(repo, Some("owner/repo"));
                    assert_eq!(action, ViolationAction::Kill);
                    targets.borrow_mut().push(("enforce", target.to_owned()));
                    None
                },
            },
        );

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].container_name, name);
        assert_eq!(violations[0].repo.as_deref(), Some("owner/repo"));
        assert!(violations[0].action_error.is_none());
        assert_eq!(
            targets.into_inner(),
            vec![
                ("processes", original_id.to_owned()),
                ("repo-label", original_id.to_owned()),
                ("enforce", original_id.to_owned()),
            ]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redacts_command_arguments_before_logging() {
        assert_eq!(
            redact_command_line("git push --token super-secret"),
            "git [arguments redacted]"
        );
        assert_eq!(redact_command_line("bash"), "bash");
        assert_eq!(redact_command_line("   "), "<empty command>");
    }

    #[test]
    fn ban_duration_saturates_instead_of_overflowing() {
        let dir = temp_state_dir("overflow");
        let mut store = BanStore::load(&dir).unwrap();
        store
            .ban("owner/repo", Duration::from_secs(u64::MAX))
            .unwrap();
        assert!(store.is_banned("owner/repo"));
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
