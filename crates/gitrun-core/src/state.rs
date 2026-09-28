use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("state I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("state serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HealthReport {
    pub healthy: bool,
    pub checked_at: u64,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn health_path(&self) -> PathBuf {
        self.root.join("health.json")
    }
    pub fn crash_path(&self) -> PathBuf {
        self.root.join("last-crash")
    }

    pub fn write_health(
        &self,
        healthy: bool,
        message: impl Into<String>,
    ) -> Result<(), StateError> {
        fs::create_dir_all(&self.root)?;
        let report = HealthReport {
            healthy,
            checked_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            message: message.into(),
        };
        let tmp = self
            .root
            .join(format!("health.json.tmp.{}", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(&tmp, self.health_path())?;
        Ok(())
    }
    pub fn record_crash(&self, message: impl Into<String>) -> Result<(), StateError> {
        fs::create_dir_all(&self.root)?;
        fs::write(self.crash_path(), message.into())?;
        Ok(())
    }
    pub fn read_health(&self) -> Result<Option<HealthReport>, StateError> {
        match fs::read_to_string(self.health_path()) {
            Ok(value) => Ok(Some(serde_json::from_str(&value)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn read_last_crash(&self) -> Result<Option<String>, StateError> {
        match fs::read_to_string(self.crash_path()) {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    #[test]
    fn health_and_crash_state_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "gitrun-state-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = StateStore::new(&root);
        store.write_health(true, "ok").unwrap();
        store.record_crash("example failure").unwrap();
        let health = store.read_health().unwrap().unwrap();
        assert!(health.healthy);
        assert_eq!(health.message, "ok");
        assert_eq!(
            store.read_last_crash().unwrap().as_deref(),
            Some("example failure")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
