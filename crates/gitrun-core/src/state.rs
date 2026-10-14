use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static STATE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
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
        self.write_atomic("health.json", &serde_json::to_vec_pretty(&report)?)
    }

    pub fn record_crash(&self, message: impl Into<String>) -> Result<(), StateError> {
        fs::create_dir_all(&self.root)?;
        self.write_atomic("last-crash", message.into().as_bytes())
    }

    fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<(), StateError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = STATE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.root.join(format!(
            "{name}.tmp.{}.{nonce}.{sequence}",
            std::process::id()
        ));
        let write_result = (|| -> Result<(), StateError> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, self.root.join(name))?;

            #[cfg(unix)]
            fs::File::open(&self.root)?.sync_all()?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        write_result
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
