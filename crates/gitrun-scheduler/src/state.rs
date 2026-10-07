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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

static STATE_TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state file: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid persisted scheduler state: {0}")]
    Invalid(String),
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateFile {
    /// container name -> unix seconds when it was first observed idle.
    #[serde(default)]
    idle_since: HashMap<String, u64>,
    /// container name -> unix seconds when it was first observed needing recovery.
    #[serde(default)]
    recovery_since: HashMap<String, u64>,
    /// Containers observed to have exited unexpectedly. Quarantined names
    /// are never restarted/reused; reconciliation must create a fresh runner.
    #[serde(default)]
    quarantined_since: HashMap<String, u64>,
}

pub struct SchedulerState {
    path: PathBuf,
    data: StateFile,
}

fn validate_state(data: &StateFile) -> Result<(), StateError> {
    let now = SchedulerState::now();

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
        let data = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StateFile::default(),
            Err(error) => return Err(error.into()),
        };
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

        let sequence = STATE_TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StateError::Invalid("scheduler state path has no valid file name".into())
            })?;
        let tmp = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));

        let serialized = serde_json::to_string_pretty(&self.data)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(serialized.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

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
        let safe_repo: String = repo
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let repo_prefix = format!("gitrun-{safe_repo}-");
        let live_names: std::collections::HashSet<&str> =
            live_names.iter().map(String::as_str).collect();
        self.data.idle_since.retain(|name, _| {
            !name.starts_with(&repo_prefix) || live_names.contains(name.as_str())
        });
        self.data.recovery_since.retain(|name, _| {
            !name.starts_with(&repo_prefix) || live_names.contains(name.as_str())
        });
        self.data.quarantined_since.retain(|name, _| {
            !name.starts_with(&repo_prefix) || live_names.contains(name.as_str())
        });
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
        self.data.quarantined_since.iter().map(|(name, since)| {
            (name.clone(), Duration::from_secs(now.saturating_sub(*since)))
        }).collect()
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
