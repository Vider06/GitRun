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
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Active JSONL stays bounded; archives provide a finite, ordered audit window.
const MAX_QUEUE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVES: usize = 10;
static ARCHIVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
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

/// Cross-process queue lock. Writers, rotation, and readers use the same
/// sidecar lock so a reader cannot observe the queue between archive rename
/// and the next append.
struct QueueLock(File);

impl QueueLock {
    fn acquire(path: &Path, exclusive: bool) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_name))?;
        #[cfg(unix)]
        {
            let mode = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
            // SAFETY: flock operates on the live descriptor owned by lock.
            if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), mode) } == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(unix))]
        let _ = exclusive;
        Ok(Self(lock))
    }
}

impl Drop for QueueLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: the descriptor remains open for the lifetime of this guard.
            unsafe {
                libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
            }
        }
    }
}

fn archive_prefix(path: &Path) -> Option<String> {
    Some(format!("{}.archive-", path.file_name()?.to_str()?))
}

/// Returns archive paths in oldest-to-newest lexical order.
fn archive_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let Some(prefix) = archive_prefix(path) else {
        return Ok(Vec::new());
    };
    let mut archives = Vec::new();
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(archives),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix))
            && entry.file_type()?.is_file()
        {
            archives.push(entry.path());
        }
    }
    archives.sort_by(|left, right| {
        let left_modified = std::fs::metadata(left).and_then(|metadata| metadata.modified()).unwrap_or(UNIX_EPOCH);
        let right_modified = std::fs::metadata(right).and_then(|metadata| metadata.modified()).unwrap_or(UNIX_EPOCH);
        left_modified.cmp(&right_modified).then_with(|| left.cmp(right))
    });
    Ok(archives)
}

fn prune_archives(path: &Path) -> Result<()> {
    let archives = archive_paths(path)?;
    let excess = archives.len().saturating_sub(MAX_ARCHIVES);
    for archive in archives.into_iter().take(excess) {
        std::fs::remove_file(archive)?;
    }
    Ok(())
}

fn rotate_queue(path: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() == 0 {
        return Ok(());
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = ARCHIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let archive = path.with_file_name(format!(
        "{}.archive-{timestamp:020}-{:010}-{sequence:010}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("events"),
        std::process::id()
    ));
    std::fs::rename(path, archive)?;
    prune_archives(path)
}

/// Appends an event and rotates the active JSONL file before it exceeds 16 MiB.
/// Up to ten archives are retained. The advisory lock coordinates current
/// GitRun producers and readers across processes.
pub fn emit(path: &Path, event: &SecurityEvent) -> Result<()> {
    let line = serde_json::to_string(event)?;
    let _lock = QueueLock::acquire(path, true)?;
    if std::fs::metadata(path)
        .map(|metadata| metadata.len().saturating_add(line.len() as u64 + 1) > MAX_QUEUE_BYTES)
        .unwrap_or(false)
    {
        rotate_queue(path)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn read_file_all(path: &Path) -> Result<Vec<SecurityEvent>> {
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

/// Reads retained archives followed by the active queue under a shared lock.
pub fn read_all(path: &Path) -> Result<Vec<SecurityEvent>> {
    let _lock = QueueLock::acquire(path, false)?;
    let mut events = Vec::new();
    for archive in archive_paths(path)? {
        events.extend(read_file_all(&archive)?);
    }
    events.extend(read_file_all(path)?);
    Ok(events)
}

/// Counts critical events across the retained audit window with bounded memory.
pub fn count_critical_since(path: &Path, cutoff: u64) -> Result<u32> {
    let _lock = QueueLock::acquire(path, false)?;
    let mut count = 0_u32;
    let mut files = archive_paths(path)?;
    files.push(path.to_path_buf());
    for queue in files {
        let file = match File::open(&queue) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
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
    }
    Ok(count)
}

/// Reads the newest records across archives and the active file without
/// loading the full retained history into memory.
pub fn read_last(path: &Path, limit: usize) -> Result<Vec<SecurityEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let _lock = QueueLock::acquire(path, false)?;
    let mut files = archive_paths(path)?;
    files.push(path.to_path_buf());
    let mut result = Vec::new();
    for queue in files.into_iter().rev() {
        let remaining = limit.saturating_sub(result.len());
        if remaining == 0 {
            break;
        }
        let mut older = read_last_file(&queue, remaining)?;
        older.append(&mut result);
        result = older;
    }
    Ok(result)
}

fn read_last_file(path: &Path, limit: usize) -> Result<Vec<SecurityEvent>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut position = file.metadata()?.len();
    if position == 0 {
        return Ok(Vec::new());
    }

    let mut last_byte = [0_u8; 1];
    file.seek(SeekFrom::Start(position - 1))?;
    file.read_exact(&mut last_byte)?;
    let trailing_newline = last_byte[0] == b'\n';
    let separators_before_cutoff = limit.saturating_sub(1) + if trailing_newline { 1 } else { 0 };

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
    fn rotation_preserves_order_for_all_read_paths() {
        let path = temp_path("rotation");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
        emit(
            &path,
            &SecurityEvent::new("test", Severity::Critical, "before rotation"),
        )
        .unwrap();
        {
            let _lock = QueueLock::acquire(&path, true).unwrap();
            rotate_queue(&path).unwrap();
        }
        emit(
            &path,
            &SecurityEvent::new("test", Severity::Info, "after rotation"),
        )
        .unwrap();

        let all = read_all(&path).unwrap();
        assert_eq!(
            all.iter()
                .map(|event| event.message.as_str())
                .collect::<Vec<_>>(),
            vec!["before rotation", "after rotation"]
        );
        assert_eq!(read_last(&path, 1).unwrap()[0].message, "after rotation");
        assert_eq!(count_critical_since(&path, 0).unwrap(), 1);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
        for archive in archive_paths(&path).unwrap() {
            let _ = std::fs::remove_file(archive);
        }
    }

    #[test]
    fn archive_retention_prunes_oldest_archives() {
        let path = temp_path("retention");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
        let prefix = format!("{}.archive-", path.file_name().unwrap().to_string_lossy());
        for index in 0..(MAX_ARCHIVES + 2) {
            let archive = path.with_file_name(format!("{prefix}{index:020}-0000000000-0000000000"));
            std::fs::write(archive, "archive\n").unwrap();
        }

        prune_archives(&path).unwrap();
        let archives = archive_paths(&path).unwrap();
        assert_eq!(archives.len(), MAX_ARCHIVES);
        assert!(!archives[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("00000000000000000000-"));
        for archive in archives {
            let _ = std::fs::remove_file(archive);
        }
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }

    #[test]
    fn concurrent_writers_keep_complete_jsonl_records() {
        let path = temp_path("concurrent");
        let _ = std::fs::remove_file(&path);
        let mut writers = Vec::new();
        for writer in 0..4 {
            let path = path.clone();
            writers.push(std::thread::spawn(move || {
                for index in 0..25 {
                    emit(
                        &path,
                        &SecurityEvent::new(
                            format!("writer-{writer}"),
                            Severity::Info,
                            format!("event-{index}"),
                        ),
                    )
                    .unwrap();
                }
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        let events = read_all(&path).unwrap();
        assert_eq!(events.len(), 100);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }

    #[test]
    fn severity_ordering_places_critical_highest() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }
}
