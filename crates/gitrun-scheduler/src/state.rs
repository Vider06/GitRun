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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state file: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateFile {
    /// container name -> unix seconds when it was first observed idle.
    #[serde(default)]
    idle_since: HashMap<String, u64>,
    /// container name -> unix seconds when it was first observed needing recovery.
    #[serde(default)]
    recovery_since: HashMap<String, u64>,
}

pub struct SchedulerState {
    path: PathBuf,
    data: StateFile,
}

impl SchedulerState {
    pub fn load(state_dir: &Path) -> Result<Self, StateError> {
        let path = state_dir.join("scheduler-state.json");
        let data = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StateFile::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, data })
    }

    pub fn save(&self) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.data)?)?;
        fs::rename(&tmp, &self.path)?;
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
    pub fn prune(&mut self, live_names: &[String]) {
        self.data
            .idle_since
            .retain(|name, _| live_names.contains(name));
        self.data
            .recovery_since
            .retain(|name, _| live_names.contains(name));
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
    fn prune_drops_entries_for_gone_containers() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-sched-state-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut state = SchedulerState::load(&dir).unwrap();
        state.mark_idle("runner-a");
        state.mark_idle("runner-b");
        state.prune(&["runner-a".to_owned()]);
        let names: Vec<_> = state.idle_entries().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["runner-a".to_owned()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
