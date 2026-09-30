//! Shared protocol for the "KVM isn't available, retry or fall back to
//! VirtualBox?" decision, asked by `gitrun-autoscaler` (headless, no
//! terminal) and answered by the operator through the dashboard.
//!
//! `gitrun-autoscaler` and the dashboard are two separate processes with no
//! IPC channel between them today, and the dashboard isn't necessarily
//! running when the autoscaler needs an answer. Rather than a live
//! socket/channel (which would require the dashboard to be open at the
//! exact moment of the request), this uses the same durable, file-based
//! approach as `gitrun-gsr`'s event queue: one small JSON file per pending
//! VM setup, under `{state_dir}/hypervisor-decisions/{vm_name}.json`.
//! Writes are serialized per VM and committed through a temporary file plus
//! rename, so readers never observe a partially written JSON document.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const LOCK_WAIT: Duration = Duration::from_millis(20);
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid decision record: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DecisionError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionChoice {
    RetryKvm,
    UseVirtualBox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDecision {
    pub vm_name: String,
    pub error: String,
    pub requested_at: u64,
    pub choice: Option<DecisionChoice>,
    pub responded_at: Option<u64>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn decisions_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("hypervisor-decisions")
}

fn decision_filename(vm_name: &str) -> String {
    let mut encoded = String::new();
    for byte in vm_name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("_{byte:02x}"));
        }
    }
    if encoded.is_empty() {
        encoded.push_str("_empty");
    }
    format!("{encoded}.json")
}

fn decision_path(state_dir: &Path, vm_name: &str) -> PathBuf {
    decisions_dir(state_dir).join(decision_filename(vm_name))
}

fn lock_path(state_dir: &Path, vm_name: &str) -> PathBuf {
    decision_path(state_dir, vm_name).with_extension("lock")
}

struct DecisionLock {
    path: PathBuf,
}

impl Drop for DecisionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(state_dir: &Path, vm_name: &str) -> Result<DecisionLock> {
    let path = lock_path(state_dir, vm_name);
    let started = std::time::Instant::now();

    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path) {
            Ok(_) => return Ok(DecisionLock { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default() > STALE_LOCK_AGE {
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                    }
                }
                if started.elapsed() >= LOCK_TIMEOUT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("timed out waiting for hypervisor decision lock for VM '{vm_name}'"),
                    )
                    .into());
                }
                thread::sleep(LOCK_WAIT);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_record(path: &Path, record: &PendingDecision) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("decision path has no parent directory"))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("decision"),
        std::process::id(),
        unique
    ));
    fs::write(&tmp, serde_json::to_string(record)?)?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

// Atomic lock/record handling keeps dashboard responses durable across process boundaries.
pub fn request(state_dir: &Path, vm_name: &str, error: &str) -> Result<()> {
    let dir = decisions_dir(state_dir);
    fs::create_dir_all(&dir)?;
    let _lock = acquire_lock(state_dir, vm_name)?;
    write_record(
        &decision_path(state_dir, vm_name),
        &PendingDecision {
            vm_name: vm_name.to_owned(),
            error: error.to_owned(),
            requested_at: now(),
            choice: None,
            responded_at: None,
        },
    )
}

pub fn respond(state_dir: &Path, vm_name: &str, choice: DecisionChoice) -> Result<()> {
    let _lock = acquire_lock(state_dir, vm_name)?;
    let path = decision_path(state_dir, vm_name);
    let mut record: PendingDecision = serde_json::from_str(&fs::read_to_string(&path)?)?;
    record.choice = Some(choice);
    record.responded_at = Some(now());
    write_record(&path, &record)
}

pub fn poll(state_dir: &Path, vm_name: &str) -> Result<Option<PendingDecision>> {
    match fs::read_to_string(decision_path(state_dir, vm_name)) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn clear(state_dir: &Path, vm_name: &str) -> Result<()> {
    let _lock = acquire_lock(state_dir, vm_name)?;
    match fs::remove_file(decision_path(state_dir, vm_name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn list_pending(state_dir: &Path) -> Result<Vec<PendingDecision>> {
    let dir = decisions_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut pending = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let record: PendingDecision = serde_json::from_str(&raw)?;
        if record.choice.is_none() {
            pending.push(record);
        }
    }
    pending.sort_by_key(|d| d.requested_at);
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-hv-decision-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn request_then_poll_sees_pending_with_no_choice() {
        let dir = temp_dir("pending");
        request(&dir, "win-runner-1", "virsh: connection refused").unwrap();
        let seen = poll(&dir, "win-runner-1").unwrap().expect("record should exist");
        assert_eq!(seen.vm_name, "win-runner-1");
        assert!(seen.choice.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn respond_then_poll_sees_the_choice() {
        let dir = temp_dir("respond");
        request(&dir, "win-runner-1", "boom").unwrap();
        respond(&dir, "win-runner-1", DecisionChoice::UseVirtualBox).unwrap();
        let seen = poll(&dir, "win-runner-1").unwrap().expect("record should exist");
        assert_eq!(seen.choice, Some(DecisionChoice::UseVirtualBox));
        assert!(seen.responded_at.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_on_missing_record_is_none_not_an_error() {
        let dir = temp_dir("missing");
        assert!(poll(&dir, "nonexistent").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_the_record() {
        let dir = temp_dir("clear");
        request(&dir, "win-runner-1", "boom").unwrap();
        clear(&dir, "win-runner-1").unwrap();
        assert!(poll(&dir, "win-runner-1").unwrap().is_none());
        clear(&dir, "win-runner-1").unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_pending_excludes_answered_records() {
        let dir = temp_dir("list");
        request(&dir, "vm-a", "err-a").unwrap();
        request(&dir, "vm-b", "err-b").unwrap();
        respond(&dir, "vm-b", DecisionChoice::RetryKvm).unwrap();
        let pending = list_pending(&dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].vm_name, "vm-a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_pending_on_missing_dir_is_empty_not_an_error() {
        let dir = temp_dir("missing-dir");
        assert!(list_pending(&dir).unwrap().is_empty());
    }

    #[test]
    fn vm_names_have_collision_free_safe_filenames() {
        assert_ne!(decision_filename("foo/bar"), decision_filename("foo_bar"));
        assert_ne!(decision_filename(""), decision_filename("_empty"));
        assert!(decision_filename("../../etc/passwd").ends_with(".json"));
    }

    #[test]
    fn response_waits_for_existing_writer() {
        let dir = temp_dir("lock");
        request(&dir, "vm-a", "boom").unwrap();
        let lock = acquire_lock(&dir, "vm-a").unwrap();
        let result = {
            let dir = dir.clone();
            std::thread::spawn(move || respond(&dir, "vm-a", DecisionChoice::RetryKvm))
                .join()
                .expect("response thread")
        };
        assert!(matches!(
            result,
            Err(DecisionError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut
        ));
        drop(lock);
        let _ = fs::remove_dir_all(&dir);
    }
}
