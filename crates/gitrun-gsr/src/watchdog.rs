//! Process watchdog: the first concrete piece of GSR's "external, separate
//! process" design (decided explicitly with the operator: a thread inside
//! the watched process can't observe that process's own hard crash — only a
//! genuinely separate process can).
//!
//! What this does today: polls whether a PID is alive, and when it
//! disappears unexpectedly (no clean shutdown marker written), logs the
//! event and attempts to show an error to the operator — graphically if
//! possible, falling back to a terminal message, always to the log. This is
//! the "crash/error handler" half of GSR's scope; the "hardening" half
//! (container escape prevention, command whitelisting) is a separate,
//! larger piece of work not started in this pass — see the crate-level docs
//! for the full scope split.
//!
//! PID liveness check: same `kill(pid, 0)` technique already used in
//! `gitrun-scheduler`'s GTUU stale-lock detection (signal 0 delivers
//! nothing, only checks existence/permission — see that module for the
//! detailed rationale), duplicated here rather than shared because pulling
//! in `gitrun-scheduler` as a dependency of `gitrun-gsr` would be backwards:
//! GSR is meant to supervise the scheduler, not depend on it.

use crate::events::{self, Severity};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc_kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM: alive, just not ours
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

pub struct WatchConfig {
    /// Path to a file containing the watched process's PID as plain text —
    /// the watched process (e.g. `gitrun-autoscaler`) is responsible for
    /// writing this on startup and removing it on clean shutdown. A PID
    /// file that exists but names a dead process means an unclean exit
    /// (crash), which is exactly the condition this watchdog exists to
    /// catch — a missing PID file means a clean shutdown or the process
    /// simply hasn't started yet, neither of which is an incident.
    pub pid_file: std::path::PathBuf,
    pub events_path: std::path::PathBuf,
    pub poll_interval: Duration,
    /// Human-readable name for the watched process, used in log/error
    /// messages (e.g. "gitrun-autoscaler").
    pub watched_name: String,
}

/// Runs the watchdog loop until `should_stop` returns true. Blocking; the
/// caller (GSR's `main.rs`) runs this on its own thread or as the entire
/// process's main loop.
pub fn run(config: &WatchConfig, should_stop: impl Fn() -> bool) {
    let mut last_known_pid: Option<u32> = None;

    while !should_stop() {
        match read_pid(&config.pid_file) {
            Some(pid) => {
                if process_is_alive(pid) {
                    last_known_pid = Some(pid);
                } else if last_known_pid == Some(pid) {
                    // The PID file still names a process we previously saw
                    // alive, and now it's gone without the file having been
                    // cleaned up — that's an unclean exit (crash), not a
                    // deliberate shutdown (which removes the file).
                    handle_crash(config, pid);
                    last_known_pid = None;
                }
            }
            None => {
                // No PID file: either not started yet, or shut down cleanly.
                // Neither is a crash; just wait for the next poll.
                last_known_pid = None;
            }
        }
        std::thread::sleep(config.poll_interval);
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn handle_crash(config: &WatchConfig, pid: u32) {
    let message = format!("{} (pid {pid}) exited unexpectedly", config.watched_name);

    // Always log first — this is the one action that must never fail
    // silently, since it's the only record if the graphical/terminal
    // notification below also fails.
    let event = crate::events::SecurityEvent::new("gsr-watchdog", Severity::Critical, message.clone());
    if let Err(error) = events::emit(&config.events_path, &event) {
        eprintln!("gitrun-gsr: CRITICAL: {message} (additionally failed to write event log: {error})");
    }

    show_error(&message);
}

/// Shows an error to the operator: graphically if a display and a notifier
/// are available, falling back to a plain terminal message otherwise.
/// Logging (above, in `handle_crash`) always happens regardless of whether
/// this succeeds — per the operator's design ("shows the error graphically
/// if it fails... otherwise falls back to terminal, and writes logs").
fn show_error(message: &str) {
    if try_graphical_notification(message) {
        return;
    }
    eprintln!("gitrun-gsr: {message}");
}

/// Attempts a desktop notification via `notify-send` (present on most Linux
/// desktop environments with a notification daemon running). Returns false
/// — falling through to the terminal message — if `notify-send` isn't
/// available or the environment has no display/notification daemon (e.g. a
/// headless server, which is a completely normal GitRun deployment target,
/// not an error condition in itself).
fn try_graphical_notification(message: &str) -> bool {
    Command::new("notify-send")
        .arg("GitRun — process crashed")
        .arg(message)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_parses_valid_file() {
        let path = std::env::temp_dir().join(format!("gitrun-gsr-pid-test-{}.pid", std::process::id()));
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(read_pid(&path), Some(12345));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let path = std::env::temp_dir().join(format!("gitrun-gsr-pid-missing-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_pid(&path), None);
    }

    #[test]
    fn read_pid_returns_none_for_garbage_content() {
        let path = std::env::temp_dir().join(format!("gitrun-gsr-pid-garbage-{}.pid", std::process::id()));
        std::fs::write(&path, "not-a-pid").unwrap();
        assert_eq!(read_pid(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn own_process_is_reported_alive() {
        // The watchdog's own PID is, definitionally, always alive while this
        // test runs — exercises the "true" path of process_is_alive without
        // needing to spawn/kill a real subprocess.
        assert!(process_is_alive(std::process::id()));
    }

    #[test]
    fn crash_of_dead_pid_is_logged() {
        let events_path = std::env::temp_dir().join(format!("gitrun-gsr-crash-events-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&events_path);
        let config = WatchConfig {
            pid_file: std::env::temp_dir().join("unused.pid"),
            events_path: events_path.clone(),
            poll_interval: Duration::from_millis(10),
            watched_name: "test-process".into(),
        };
        handle_crash(&config, 999999);
        let events = events::read_all(&events_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, Severity::Critical);
        assert!(events[0].message.contains("test-process"));
        let _ = std::fs::remove_file(&events_path);
    }
}
