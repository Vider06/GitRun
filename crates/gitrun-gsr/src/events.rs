//! Security event model: the shared vocabulary GSR uses to receive signals
//! from every other GitRun module (GitVault, the scheduler, setup, etc.)
//! without those modules depending on GSR directly.
//!
//! Why a file-based queue rather than a socket/IPC channel: GSR must be able
//! to observe events even if it starts *after* the event happened (e.g. GSR
//! itself was down when a tamper attempt occurred, then comes back up) — a
//! live-only channel (Unix socket, in-memory channel) would lose anything
//! emitted while no GSR process was listening. A simple append-only JSONL
//! file, read from an offset, survives GSR restarts and doesn't require
//! GSR to be running at write time. This trades real-time delivery for
//! durability — appropriate for a hardening layer that's meant to notice
//! and record incidents, not react within milliseconds.
//!
//! `gitrun-vault`'s `VaultEventSink` trait (see that crate) is the first
//! concrete producer: an implementation of it that writes to this queue
//! plugs GitVault's tamper detection into GSR without gitrun-vault knowing
//! GSR exists.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid event record: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EventError>;

/// Severity, used by GSR to decide how loudly to react (log only vs. also
/// surface an error dialog/terminal message per the operator's "shows the
/// error graphically, falls back to terminal + always logs" design).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    /// Something that looks like an actual attack or tampering attempt —
    /// e.g. GitVault's authenticated decryption failing on ciphertext that
    /// was previously written successfully (see `gitrun-vault`'s
    /// `VaultEventSink::on_decryption_failure` doc comment).
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub timestamp: u64,
    pub source: String,
    pub severity: Severity,
    pub message: String,
}

impl SecurityEvent {
    pub fn new(source: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            source: source.into(),
            severity,
            message: message.into(),
        }
    }
}

/// Appends a security event to the shared queue file at `path`. Safe to
/// call from any GitRun process/thread: opens in append mode, so concurrent
/// writers from different processes don't corrupt each other's records as
/// long as each write is a single line (guaranteed here — events are
/// serialized to one JSON line each, and POSIX guarantees small
/// (<PIPE_BUF, typically 4KB) writes via O_APPEND don't interleave).
pub fn emit(path: &Path, event: &SecurityEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(event)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Reads every event currently in the queue file. GSR's watchdog loop calls
/// this periodically and tracks its own read offset (see `main.rs`) rather
/// than this function tracking state, so multiple readers (or a restarted
/// GSR) can each maintain their own position independently.
pub fn read_all(path: &Path) -> Result<Vec<SecurityEvent>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(EventError::from))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Default path for the shared event queue, under a state directory (the
/// same `state_dir` concept used elsewhere in GitRun's config).
pub fn default_queue_path(state_dir: &str) -> PathBuf {
    PathBuf::from(state_dir).join("gsr-events.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gitrun-gsr-events-test-{label}-{}.jsonl", std::process::id()))
    }

    #[test]
    fn emit_and_read_round_trip() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        emit(&path, &SecurityEvent::new("test", Severity::Info, "hello")).unwrap();
        let events = read_all(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "hello");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(read_all(&path).unwrap().is_empty());
    }

    #[test]
    fn multiple_events_append_in_order() {
        let path = temp_path("multi");
        let _ = std::fs::remove_file(&path);
        emit(&path, &SecurityEvent::new("a", Severity::Info, "first")).unwrap();
        emit(&path, &SecurityEvent::new("b", Severity::Critical, "second")).unwrap();
        let events = read_all(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[1].severity, Severity::Critical);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn severity_ordering_places_critical_highest() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }
}
