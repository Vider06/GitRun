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
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
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
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
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

/// Counts critical events at or after a timestamp using a streaming reader.
/// The whole durable queue is scanned to preserve exact counts, but only one
/// record is held in memory at a time.
pub fn count_critical_since(path: &Path, cutoff: u64) -> Result<u32> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };

    let mut count = 0_u32;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: SecurityEvent = serde_json::from_str(&line)?;
        if event.severity == Severity::Critical && event.timestamp >= cutoff {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Reads only the newest limit records by seeking from the end of the JSONL
/// queue. This keeps dashboard log rendering bounded even when the durable
/// event queue has grown over months of operation.
pub fn read_last(path: &Path, limit: usize) -> Result<Vec<SecurityEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut position = file.metadata()?.len();
    if position == 0 {
        return Ok(Vec::new());
    }

    // A final newline terminates the newest record; it is not an extra record.
    let mut last_byte = [0_u8; 1];
    file.seek(SeekFrom::Start(position - 1))?;
    file.read_exact(&mut last_byte)?;
    let trailing_newline = last_byte[0] == b'\n';
    let separators_before_cutoff = limit.saturating_sub(1) + usize::from(trailing_newline);

    const CHUNK_SIZE: u64 = 64 * 1024;
    let mut separators = 0usize;
    let mut reversed = Vec::new();
    while position > 0 {
        let chunk_len = position.min(CHUNK_SIZE) as usize;
        position -= chunk_len as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0_u8; chunk_len];
        file.read_exact(&mut chunk)?;

        let mut reached_cutoff = false;
        for byte in chunk.iter().rev() {
            if *byte == b'\n' && separators >= separators_before_cutoff {
                reached_cutoff = true;
                break;
            }
            if *byte == b'\n' {
                separators += 1;
            }
            reversed.push(*byte);
        }
        if reached_cutoff {
            break;
        }
    }

    reversed.reverse();
    let raw = String::from_utf8_lossy(&reversed);
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(EventError::from))
        .collect()
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
        std::env::temp_dir().join(format!(
            "gitrun-gsr-events-test-{label}-{}.jsonl",
            std::process::id()
        ))
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
        emit(
            &path,
            &SecurityEvent::new("b", Severity::Critical, "second"),
        )
        .unwrap();
        let events = read_all(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[1].severity, Severity::Critical);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn count_critical_since_counts_only_matching_events() {
        let path = temp_path("critical-count");
        let _ = std::fs::remove_file(&path);
        emit(
            &path,
            &SecurityEvent::new("test", Severity::Info, "ordinary"),
        )
        .unwrap();
        emit(
            &path,
            &SecurityEvent::new("test", Severity::Critical, "critical"),
        )
        .unwrap();

        assert_eq!(count_critical_since(&path, 0).unwrap(), 1);
        assert_eq!(count_critical_since(&path, u64::MAX).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_last_returns_newest_records_in_file_order() {
        let path = temp_path("tail");
        let _ = std::fs::remove_file(&path);
        for message in ["first", "second", "third", "fourth"] {
            emit(&path, &SecurityEvent::new("test", Severity::Info, message)).unwrap();
        }

        let events = read_last(&path, 2).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.message.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "fourth"]
        );
        assert!(read_last(&path, 0).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_last_handles_a_file_without_a_trailing_newline() {
        let path = temp_path("tail-no-newline");
        let _ = std::fs::remove_file(&path);
        let raw = [
            SecurityEvent::new("test", Severity::Info, "first"),
            SecurityEvent::new("test", Severity::Info, "second"),
            SecurityEvent::new("test", Severity::Info, "third"),
        ]
        .into_iter()
        .map(|event| serde_json::to_string(&event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, raw).unwrap();

        let events = read_last(&path, 2).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.message.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "third"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn severity_ordering_places_critical_highest() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }
}
