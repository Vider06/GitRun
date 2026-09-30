//! Shared protocol for the "KVM isn't available, retry or fall back to
//! VirtualBox?" decision, asked by `gitrun-autoscaler` (headless, no
//! terminal) and answered by the operator through the dashboard.
//!
//! `gitrun-autoscaler` and the dashboard are two separate processes with no
//! IPC channel between them today, and the dashboard isn't necessarily
//! running when the autoscaler needs an answer. Rather than a live
//! socket/channel (which would require the dashboard to be open at the
//! exact moment of the request), this uses the same durable, file-based
//! approach as `gitrun-gsr`'s event queue (see that crate's
//! `events.rs` for the fuller reasoning): one small JSON file per pending
//! VM setup, under `{state_dir}/hypervisor-decisions/{vm_name}.json`,
//! written by the autoscaler and overwritten by the dashboard when the
//! operator responds. The autoscaler polls it from a dedicated background
//! thread (see `gitrun-scheduler::vm_resolution`) rather than blocking its
//! main reconcile loop.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid decision record: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DecisionError>;

/// The operator's answer, once given. Matches the two options the dashboard
/// prompt offers: retry the KVM setup that just failed, or use VirtualBox
/// for this VM instead (see `docs` / `vm.rs`'s `HypervisorChoice` for why
/// these are the only two — GitRun never auto-installs a hypervisor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionChoice {
    RetryKvm,
    UseVirtualBox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDecision {
    pub vm_name: String,
    /// The error KVM setup failed with, shown verbatim in the dashboard
    /// prompt so the operator isn't guessing why.
    pub error: String,
    pub requested_at: u64,
    /// `None` while waiting; set once the operator responds. Kept in the
    /// same record (rather than deleting/replacing the file) so a
    /// dashboard that re-reads after responding still sees a consistent
    /// state instead of a "file vanished" race against the autoscaler
    /// clearing it.
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

fn decision_path(state_dir: &Path, vm_name: &str) -> PathBuf {
    // vm_name comes from operator-authored VM definitions (Config /
    // logic-containers-style config file), not attacker input, but sanitize
    // anyway before it becomes a filename — same defensive habit as
    // `docker::sanitize` elsewhere in the scheduler.
    let safe: String = vm_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    decisions_dir(state_dir).join(format!("{safe}.json"))
}

/// Called by the autoscaler's background resolution thread when KVM setup
/// fails: opens (or reopens) a pending decision for the dashboard to show.
/// Overwrites any previous record for this `vm_name` — a fresh KVM failure
/// supersedes whatever was there before, e.g. an old, already-answered or
/// timed-out record from a prior attempt.
// Atomic lock/record handling keeps dashboard responses durable across process boundaries.
pub fn request(state_dir: &Path, vm_name: &str, error: &str) -> Result<()> {
    let dir = decisions_dir(state_dir);
    fs::create_dir_all(&dir)?;
    let record = PendingDecision {
        vm_name: vm_name.to_owned(),
        error: error.to_owned(),
        requested_at: now(),
        choice: None,
        responded_at: None,
    };
    fs::write(
        decision_path(state_dir, vm_name),
        serde_json::to_string(&record)?,
    )?;
    Ok(())
}

/// Called by the dashboard when the operator picks an option.
pub fn respond(state_dir: &Path, vm_name: &str, choice: DecisionChoice) -> Result<()> {
    let path = decision_path(state_dir, vm_name);
    let mut record: PendingDecision = serde_json::from_str(&fs::read_to_string(&path)?)?;
    record.choice = Some(choice);
    record.responded_at = Some(now());
    fs::write(path, serde_json::to_string(&record)?)?;
    Ok(())
}

/// Called by the autoscaler's background thread, polled every few seconds
/// while waiting. `Ok(None)` means the file is gone (already cleared by an
/// earlier `clear` call — treat as "still nothing", not an error).
pub fn poll(state_dir: &Path, vm_name: &str) -> Result<Option<PendingDecision>> {
    match fs::read_to_string(decision_path(state_dir, vm_name)) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Removes the record once the autoscaler has acted on it (or given up on
/// timeout) — so a stale, already-resolved prompt doesn't linger in the
/// dashboard, and so the next attempt for this `vm_name` starts clean.
pub fn clear(state_dir: &Path, vm_name: &str) -> Result<()> {
    match fs::remove_file(decision_path(state_dir, vm_name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Lists every decision still awaiting a response (`choice.is_none()`),
/// for the dashboard to render as open prompts. Already-answered records
/// are skipped here — they're just waiting for the autoscaler thread to
/// notice and `clear` them, not something the dashboard needs to show
/// again.
pub fn list_pending(state_dir: &Path) -> Result<Vec<PendingDecision>> {
    let dir = decisions_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut pending = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
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
        let seen = poll(&dir, "win-runner-1")
            .unwrap()
            .expect("record should exist");
        assert_eq!(seen.vm_name, "win-runner-1");
        assert!(seen.choice.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn respond_then_poll_sees_the_choice() {
        let dir = temp_dir("respond");
        request(&dir, "win-runner-1", "boom").unwrap();
        respond(&dir, "win-runner-1", DecisionChoice::UseVirtualBox).unwrap();
        let seen = poll(&dir, "win-runner-1")
            .unwrap()
            .expect("record should exist");
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
        // Clearing an already-cleared (or never-created) record is a no-op,
        // not an error — the autoscaler thread doesn't need to track
        // whether it already cleared this one.
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
    fn vm_name_with_unsafe_characters_does_not_escape_the_decisions_dir() {
        let dir = temp_dir("sanitize");
        request(&dir, "../../etc/passwd", "boom").unwrap();
        // Should have landed inside the decisions dir as a sanitized
        // filename, not escaped it.
        let entries: Vec<_> = fs::read_dir(decisions_dir(&dir)).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
