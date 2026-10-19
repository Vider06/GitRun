//! Scheduler-specific persisted state: how long each container has been
//! idle, and how long each has been in a "needs recovery" condition. This is
//! the Rust equivalent of the Python `state.json` (`idle_since`/
//! `container_recovery` dicts), kept as its own small store rather than bolted
//! onto `gitrun_core::StateStore` (which is scoped to health/crash reporting)
//! so each store's file format stays simple and single-purpose.
//!
//! Same durability pattern as `gitrun_core::state::StateStore::write_health`:
//! write to a temp file, then atomically rename over the target, so a crash
//! mid-write never leaves a half-written `scheduler-state.json` behind.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

static STATE_TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_JOB_IDENTITIES: usize = 4096;
const JOB_ID_RETENTION_SECONDS: u64 = 90 * 86_400;
const JOB_ID_REFRESH_BUCKET_SECONDS: u64 = 300;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state file: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid persisted scheduler state: {0}")]
    Invalid(String),
    #[error("unsafe scheduler state path: {0}")]
    UnsafePath(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedJobIdentity {
    repository: String,
    run_id: u64,
    job_id: u64,
    gitrun_job_id: String,
    last_seen_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    idle_since: HashMap<String, u64>,
    #[serde(default)]
    recovery_since: HashMap<String, u64>,
    #[serde(default)]
    quarantined_since: HashMap<String, u64>,
    /// Provider IDs are lookup keys only; values have an independent GitRun ID.
    #[serde(default)]
    job_identities: HashMap<String, PersistedJobIdentity>,
    #[serde(default = "default_next_job_identity_sequence")]
    next_job_identity_sequence: u64,
}

fn default_next_job_identity_sequence() -> u64 { 1 }

impl Default for StateFile {
    fn default() -> Self {
        Self {
            idle_since: HashMap::new(),
            recovery_since: HashMap::new(),
            quarantined_since: HashMap::new(),
            job_identities: HashMap::new(),
            next_job_identity_sequence: 1,
        }
    }
}

pub struct SchedulerState {
    path: PathBuf,
    data: StateFile,
}

fn validate_state_directory(path: &Path) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StateError::UnsafePath(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(StateError::UnsafePath(format!(
                "{} is group/world-writable without the sticky bit",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_state_file_metadata(metadata: &fs::Metadata, path: &Path) -> Result<(), StateError> {
    if !metadata.is_file() {
        return Err(StateError::UnsafePath(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
            return Err(StateError::UnsafePath(format!(
                "{} must have one link and no group/world write permission",
                path.display()
            )));
        }
    }
    Ok(())
}

fn read_state_file(path: &Path) -> Result<Option<StateFile>, StateError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_state_file_metadata(&file.metadata()?, path)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn valid_repository_identity(repository: &str) -> bool {
    let Some((owner, name)) = repository.trim().split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && owner.len() <= 100
        && name.len() <= 100
        && !repository.chars().any(char::is_control)
        && !repository.chars().any(char::is_whitespace)
}

fn repository_identity_hash(repository: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in repository.trim().to_ascii_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn make_gitrun_job_id(repository: &str, sequence: u64) -> Option<String> {
    if !valid_repository_identity(repository) || sequence == 0 {
        return None;
    }
    Some(format!(
        "grj-{:016x}-{sequence:016x}",
        repository_identity_hash(repository)
    ))
}

fn valid_gitrun_job_id(code: &str, repository: &str) -> bool {
    let Some((hash, sequence)) = code.strip_prefix("grj-").and_then(|part| part.split_once('-')) else {
        return false;
    };
    let Ok(sequence) = u64::from_str_radix(sequence, 16) else {
        return false;
    };
    hash.len() == 16
        && hash == format!("{:016x}", repository_identity_hash(repository))
        && sequence > 0
        && code.len() == 3 + 16 + 1 + 16
}

fn job_identity_key(repository: &str, run_id: u64, job_id: u64) -> String {
    format!("{}|{run_id}|{job_id}", repository.trim().to_ascii_lowercase())
}

fn validate_state(data: &StateFile) -> Result<(), StateError> {
    let now = SchedulerState::now();

    if data.job_identities.len() > MAX_JOB_IDENTITIES || data.next_job_identity_sequence == 0 {
        return Err(StateError::Invalid(
            "GitRun job identity registry exceeds its bound or has an invalid sequence".into(),
        ));
    }
    let mut seen_gitrun_ids = std::collections::HashSet::new();
    for (key, identity) in &data.job_identities {
        if !valid_repository_identity(&identity.repository)
            || identity.run_id == 0
            || identity.job_id == 0
            || identity.last_seen_epoch == 0
            || identity.last_seen_epoch > now
            || key != &job_identity_key(&identity.repository, identity.run_id, identity.job_id)
            || !valid_gitrun_job_id(&identity.gitrun_job_id, &identity.repository)
            || !seen_gitrun_ids.insert(identity.gitrun_job_id.as_str())
        {
            return Err(StateError::Invalid(
                "persisted GitRun job identity has invalid fields or duplicate IDs".into(),
            ));
        }
    }

    for (kind, entries) in [
        ("idle_since", &data.idle_since),
        ("recovery_since", &data.recovery_since),
        ("quarantined_since", &data.quarantined_since),
    ] {
        for (name, since) in entries {
            if name.trim().is_empty() || name.chars().any(char::is_control) {
                return Err(StateError::Invalid(format!(
                    "{kind} contains invalid container name"
                )));
            }
            if *since > now {
                return Err(StateError::Invalid(format!(
                    "{kind} entry for '{name}' has a future timestamp"
                )));
            }
        }
    }

    Ok(())
}

impl SchedulerState {
    pub fn load(state_dir: &Path) -> Result<Self, StateError> {
        let path = state_dir.join("scheduler-state.json");
        match fs::symlink_metadata(state_dir) {
            Ok(_) => validate_state_directory(state_dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    data: StateFile::default(),
                });
            }
            Err(error) => return Err(error.into()),
        }
        let mut data = read_state_file(&path)?.unwrap_or_default();
        if data.next_job_identity_sequence == 0 {
            data.next_job_identity_sequence = 1;
        }
        validate_state(&data)?;
        Ok(Self { path, data })
    }

    pub fn save(&self) -> Result<(), StateError> {
        validate_state(&self.data)?;
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        validate_state_directory(parent)?;

        let sequence = STATE_TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StateError::Invalid("scheduler state path has no valid file name".into())
            })?;
        // Include wall-clock entropy as well as PID and a process-local
        // sequence. A crash can leave a temp file behind; after PID reuse,
        // a PID+sequence-only name could collide and make every future save
        // fail at create_new.
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp = parent.join(format!(
            ".{file_name}.tmp-{}-{timestamp}-{sequence}",
            std::process::id()
        ));

        let serialized = serde_json::to_string_pretty(&self.data)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        let write_result = (|| -> std::io::Result<()> {
            file.write_all(serialized.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()
        })();
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }

        if let Err(error) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }

        #[cfg(unix)]
        {
            fs::File::open(parent)?.sync_all()?;
        }

        Ok(())
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Assign a durable GitRun ID to an observed queued job. Provider run/job
    /// IDs are only used to recover the same entry across polls.
    pub fn ensure_gitrun_job_id(
        &mut self,
        repository: &str,
        run_id: u64,
        job_id: u64,
        observed_at_epoch: u64,
    ) -> Result<(String, bool), StateError> {
        if !valid_repository_identity(repository)
            || run_id == 0
            || job_id == 0
            || observed_at_epoch == 0
        {
            return Err(StateError::Invalid(
                "cannot assign a GitRun job ID to invalid provider metadata".into(),
            ));
        }
        let repository = repository.trim().to_ascii_lowercase();
        let key = job_identity_key(&repository, run_id, job_id);
        let refresh_bucket =
            observed_at_epoch / JOB_ID_REFRESH_BUCKET_SECONDS * JOB_ID_REFRESH_BUCKET_SECONDS;
        if let Some(identity) = self.data.job_identities.get_mut(&key) {
            if identity.repository != repository
                || identity.run_id != run_id
                || identity.job_id != job_id
                || !valid_gitrun_job_id(&identity.gitrun_job_id, &repository)
            {
                return Err(StateError::Invalid(
                    "existing GitRun job identity conflicts with this provider job".into(),
                ));
            }
            let mut changed = false;
            if refresh_bucket > identity.last_seen_epoch {
                identity.last_seen_epoch = refresh_bucket;
                changed = true;
            }
            return Ok((identity.gitrun_job_id.clone(), changed));
        }

        let stale_before = observed_at_epoch.saturating_sub(JOB_ID_RETENTION_SECONDS);
        self.data.job_identities.retain(|_, identity| {
            identity.last_seen_epoch >= stale_before
        });
        while self.data.job_identities.len() >= MAX_JOB_IDENTITIES {
            let oldest = self.data.job_identities
                .iter()
                .min_by_key(|(_, identity)| identity.last_seen_epoch)
                .map(|(key, _)| key.clone())
                .ok_or_else(|| StateError::Invalid(
                    "GitRun job identity registry could not evict an entry".into()
                ))?;
            self.data.job_identities.remove(&oldest);
        }
        if self.data.next_job_identity_sequence == 0 {
            self.data.next_job_identity_sequence = 1;
        }
        let sequence = self.data.next_job_identity_sequence;
        let next_sequence = sequence.checked_add(1).ok_or_else(|| {
            StateError::Invalid("GitRun job identity sequence exhausted".into())
        })?;
        let gitrun_job_id = make_gitrun_job_id(&repository, sequence).ok_or_else(|| {
            StateError::Invalid("GitRun could not generate a job identity".into())
        })?;
        self.data.next_job_identity_sequence = next_sequence;
        self.data.job_identities.insert(
            key,
            PersistedJobIdentity {
                repository,
                run_id,
                job_id,
                gitrun_job_id: gitrun_job_id.clone(),
                last_seen_epoch: refresh_bucket,
            },
        );
        Ok((gitrun_job_id, true))
    }

    /// Marks a container as idle if it wasn't already tracked, and returns
    /// how long it's been idle so far.
    pub fn mark_idle(&mut self, name: &str) -> Duration {
        let now = Self::now();
        let since = *self.data.idle_since.entry(name.to_owned()).or_insert(now);
        Duration::from_secs(now.saturating_sub(since))
    }

    pub fn clear_idle(&mut self, name: &str) {
        self.data.idle_since.remove(name);
    }

    /// Removes tracking for any container name no longer present, so the
    /// state file doesn't grow forever with names of long-gone containers.
    /// Removes tracking only for containers belonging to `repo` that no longer exist.
    /// Entries belonging to other repositories remain untouched in the shared state file.
    pub fn prune(&mut self, repo: &str, live_names: &[String]) {
        let repo_prefix = crate::docker::runner_name_prefix(repo);
        // Older releases used a lossy sanitized-repository prefix. Only prune
        // those legacy entries when neither segment contains the delimiter
        // that made collisions possible; ambiguous legacy entries are retained
        // rather than accidentally deleting another repository's state.
        let legacy_prefix = repo.split_once('/').and_then(|(owner, name)| {
            (!owner.contains('-') && !name.contains('-'))
                .then(|| format!("gitrun-{}-", crate::docker::sanitize(repo, '-')))
        });
        let live_names: std::collections::HashSet<&str> =
            live_names.iter().map(String::as_str).collect();
        let belongs_to_repo = |name: &str| {
            name.starts_with(&repo_prefix)
                || legacy_prefix
                    .as_deref()
                    .is_some_and(|prefix| name.starts_with(prefix))
        };
        self.data
            .idle_since
            .retain(|name, _| !belongs_to_repo(name) || live_names.contains(name.as_str()));
        self.data
            .recovery_since
            .retain(|name, _| !belongs_to_repo(name) || live_names.contains(name.as_str()));
        self.data
            .quarantined_since
            .retain(|name, _| !belongs_to_repo(name) || live_names.contains(name.as_str()));
    }
    /// Returns how long a container has been marked as needing recovery,
    /// starting the clock now if this is the first time we've seen it.
    pub fn recovery_age(&mut self, name: &str) -> Duration {
        let now = Self::now();
        let since = *self
            .data
            .recovery_since
            .entry(name.to_owned())
            .or_insert(now);
        Duration::from_secs(now.saturating_sub(since))
    }

    pub fn clear_recovery(&mut self, name: &str) {
        self.data.recovery_since.remove(name);
    }

    pub fn quarantine(&mut self, name: &str) {
        self.data
            .quarantined_since
            .entry(name.to_owned())
            .or_insert_with(Self::now);
        self.clear_idle(name);
        self.clear_recovery(name);
    }

    pub fn is_quarantined(&self, name: &str) -> bool {
        self.data.quarantined_since.contains_key(name)
    }

    pub fn clear_quarantine(&mut self, name: &str) {
        self.data.quarantined_since.remove(name);
    }

    pub fn quarantine_entries(&self) -> Vec<(String, Duration)> {
        let now = Self::now();
        self.data
            .quarantined_since
            .iter()
            .map(|(name, since)| {
                (
                    name.clone(),
                    Duration::from_secs(now.saturating_sub(*since)),
                )
            })
            .collect()
    }

    pub fn recovery_ages(&self) -> Vec<(String, Duration)> {
        let now = Self::now();
        self.data
            .recovery_since
            .iter()
            .map(|(name, since)| {
                (
                    name.clone(),
                    Duration::from_secs(now.saturating_sub(*since)),
                )
            })
            .collect()
    }

    pub fn idle_entries(&self) -> Vec<(String, Duration)> {
        let now = Self::now();
        self.data
            .idle_since
            .iter()
            .map(|(name, since)| {
                (
                    name.clone(),
                    Duration::from_secs(now.saturating_sub(*since)),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn load_rejects_symlinked_state_file() {
        use std::os::unix::fs::symlink;
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("external.json");
        fs::write(
            &target,
            r#"{"idle_since":{},"recovery_since":{},"quarantined_since":{}}"#,
        )
        .unwrap();
        symlink(&target, dir.join("scheduler-state.json")).unwrap();
        assert!(matches!(
            SchedulerState::load(&dir),
            Err(StateError::Io(_)) | Err(StateError::UnsafePath(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_hard_linked_state_file() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-sched-state-hardlink-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("external.json");
        fs::write(
            &target,
            r#"{"idle_since":{},"recovery_since":{},"quarantined_since":{}}"#,
        )
        .unwrap();
        fs::hard_link(&target, dir.join("scheduler-state.json")).unwrap();
        assert!(matches!(
            SchedulerState::load(&dir),
            Err(StateError::UnsafePath(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_symlinked_state_directory() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!(
            "gitrun-sched-state-dir-link-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let target = base.join("real");
        let link = base.join("link");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(matches!(
            SchedulerState::load(&link),
            Err(StateError::UnsafePath(_))
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn mark_idle_is_stable_across_calls() {
        let dir = std::env::temp_dir().join(format!("gitrun-sched-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        let first = state.mark_idle("runner-a");
        std::thread::sleep(Duration::from_millis(10));
        let second = state.mark_idle("runner-a");
        // Same "since" timestamp both times (whole-second resolution), so
        // idle duration should not reset on repeated calls.
        assert!(second >= first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_idle_removes_tracking() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-clear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        state.mark_idle("runner-a");
        state.clear_idle("runner-a");
        assert!(state.idle_entries().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitrun_job_identity_is_unique_stable_and_persisted() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-job-identities-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();

        let (first, changed) = state.ensure_gitrun_job_id("owner/repo", 41, 410, 10_000).unwrap();
        assert!(changed);
        assert!(valid_gitrun_job_id(&first, "owner/repo"));
        let (repeat, changed) = state.ensure_gitrun_job_id("owner/repo", 41, 410, 10_060).unwrap();
        assert!(!changed);
        assert_eq!(repeat, first);
        let (next, changed) = state.ensure_gitrun_job_id("owner/repo", 41, 411, 10_060).unwrap();
        assert!(changed);
        assert_ne!(next, first);
        let (other_repo, _) = state.ensure_gitrun_job_id("other/repo", 41, 410, 10_060).unwrap();
        assert_ne!(other_repo, first);
        state.save().unwrap();

        let mut reloaded = SchedulerState::load(&dir).unwrap();
        let (persisted, changed) = reloaded.ensure_gitrun_job_id("owner/repo", 41, 410, 10_120).unwrap();
        assert!(!changed);
        assert_eq!(persisted, first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-sched-state-roundtrip-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        state.mark_idle("runner-a");
        state.save().unwrap();

        let reloaded = SchedulerState::load(&dir).unwrap();
        assert_eq!(reloaded.idle_entries().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_timestamps_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-future-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scheduler-state.json");
        let future = SchedulerState::now().saturating_add(3600);
        fs::write(
            &path,
            format!(r#"{{"idle_since":{{"runner-a":{future}}},"recovery_since":{{}}}}"#),
        )
        .unwrap();

        assert!(matches!(
            SchedulerState::load(&dir),
            Err(StateError::Invalid(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_container_names_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-name-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scheduler-state.json");
        fs::write(&path, r#"{"idle_since":{"":"1"},"recovery_since":{}}"#).unwrap();

        assert!(matches!(
            SchedulerState::load(&dir),
            Err(StateError::Decode(_)) | Err(StateError::Invalid(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_uses_atomic_replacement() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        state.mark_idle("runner-a");
        state.save().unwrap();

        assert!(dir.join("scheduler-state.json").is_file());
        assert!(!dir.join("scheduler-state.json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_does_not_cross_delete_repositories_with_colliding_legacy_slugs() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-sched-state-collision-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        let first_repo = "foo-bar/baz";
        let second_repo = "foo/bar-baz";
        assert_eq!(
            crate::docker::sanitize(first_repo, '-'),
            crate::docker::sanitize(second_repo, '-')
        );

        let stale_first = format!("{}gone", crate::docker::runner_name_prefix(first_repo));
        let quarantined_second = format!(
            "{}quarantined",
            crate::docker::runner_name_prefix(second_repo)
        );
        state.mark_idle(&stale_first);
        state.quarantine(&quarantined_second);
        state.prune(first_repo, &[]);

        assert!(state
            .idle_entries()
            .iter()
            .all(|(name, _)| name != &stale_first));
        assert!(state.is_quarantined(&quarantined_second));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_drops_gone_entries_without_cross_repo_deletion() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        state.mark_idle("gitrun-owner-repo-runner-a");
        state.mark_idle("gitrun-owner-repo-gone");
        state.mark_idle("gitrun-other-repo-runner-b");
        state.recovery_age("gitrun-owner-repo-recovery-gone");
        state.recovery_age("gitrun-other-repo-recovery");

        state.prune("owner/repo", &["gitrun-owner-repo-runner-a".to_owned()]);

        let idle_names: Vec<_> = state
            .idle_entries()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(idle_names.contains(&"gitrun-owner-repo-runner-a".to_owned()));
        assert!(!idle_names.contains(&"gitrun-owner-repo-gone".to_owned()));
        assert!(idle_names.contains(&"gitrun-other-repo-runner-b".to_owned()));

        let recovery_names: Vec<_> = state
            .recovery_ages()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(!recovery_names.contains(&"gitrun-owner-repo-recovery-gone".to_owned()));
        assert!(recovery_names.contains(&"gitrun-other-repo-recovery".to_owned()));

        let _ = fs::remove_dir_all(&dir);
    }
}
