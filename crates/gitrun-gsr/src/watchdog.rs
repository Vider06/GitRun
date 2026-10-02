//! Process watchdog: the first concrete piece of GSR's "external, separate
//! process" design (decided explicitly with the operator: a thread inside
//! the watched process can't observe that process's own hard crash — only a
//! genuinely separate process can).
//!
//! What this does today: watches the scheduler PID file and, once the
//! scheduler has been observed alive, keeps a stable Linux pidfd for that
//! process so PID reuse cannot make an unrelated process look healthy. If
//! the scheduler disappears unexpectedly (the PID file survives because the
//! process did not perform its clean-shutdown cleanup), GSR records a
//! critical event and attempts to show an error to the operator — graphically
//! if possible, falling back to a terminal message.
//!
//! On Unix targets without Linux pidfds, the watchdog falls back to
//! `kill(pid, 0)` liveness checks. GitRun's production deployment is Linux,
//! where the pidfd path avoids the PID-reuse race. GSR deliberately does not
//! depend on `gitrun-scheduler`: it is meant to supervise it, not depend on it.

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
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Option<i32> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    (fd >= 0).then_some(fd as i32)
}

#[cfg(target_os = "linux")]
fn pidfd_is_alive(pidfd: i32) -> bool {
    let mut pollfd = libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
    result == 0 || (result > 0 && pollfd.revents & libc::POLLIN == 0)
}

#[cfg(target_os = "linux")]
fn close_pidfd(pidfd: i32) {
    unsafe { libc::close(pidfd) };
}

pub struct WatchConfig {
    /// Path to the scheduler PID file. The Rust scheduler writes its PID on
    /// startup and removes the file on clean shutdown. A surviving PID file
    /// after the process disappears is the watchdog's crash signal.
    pub pid_file: std::path::PathBuf,
    pub events_path: std::path::PathBuf,
    pub poll_interval: Duration,
    /// Human-readable name for the watched process, used in log/error
    /// messages (currently `gitrun-autoscaler`, the scheduler's legacy
    /// process name).
    pub watched_name: String,
}

/// Runs the watchdog loop until `should_stop` returns true. Blocking; the
/// caller (GSR's `main.rs`) runs this on its own thread or as the entire
/// process's main loop.
pub fn run(config: &WatchConfig, should_stop: impl Fn() -> bool) {
    let mut last_known_pid: Option<u32> = None;
    #[cfg(target_os = "linux")]
    let mut pidfd: Option<i32> = None;

    while !should_stop() {
        match read_pid(&config.pid_file) {
            Some(pid) => {
                #[cfg(target_os = "linux")]
                {
                    if last_known_pid != Some(pid) {
                        if let Some(fd) = pidfd.take() {
                            close_pidfd(fd);
                        }
                        pidfd = open_pidfd(pid);
                    }
                    let alive = pidfd.map(pidfd_is_alive).unwrap_or(false);
                    if alive {
                        last_known_pid = Some(pid);
                    } else if last_known_pid == Some(pid) {
                        handle_crash(config, pid);
                        last_known_pid = None;
                        if let Some(fd) = pidfd.take() {
                            close_pidfd(fd);
                        }
                    }
                }

                #[cfg(not(target_os = "linux"))]
                if process_is_alive(pid) {
                    last_known_pid = Some(pid);
                } else if last_known_pid == Some(pid) {
                    handle_crash(config, pid);
                    last_known_pid = None;
                }
            }
            None => {
                last_known_pid = None;
                #[cfg(target_os = "linux")]
                if let Some(fd) = pidfd.take() {
                    close_pidfd(fd);
                }
            }
        }
        std::thread::sleep(config.poll_interval);
    }

    #[cfg(target_os = "linux")]
    if let Some(fd) = pidfd {
        close_pidfd(fd);
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
    let event =
        crate::events::SecurityEvent::new("gsr-watchdog", Severity::Critical, message.clone());
    if let Err(error) = events::emit(&config.events_path, &event) {
        eprintln!(
            "gitrun-gsr: CRITICAL: {message} (additionally failed to write event log: {error})"
        );
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
    let mut child = match Command::new("notify-send")
        .arg("GitRun — process crashed")
        .arg(message)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    const TIMEOUT: Duration = Duration::from_secs(2);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let deadline = std::time::Instant::now() + TIMEOUT;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_parses_valid_file() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-test-{}.pid", std::process::id()));
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(read_pid(&path), Some(12345));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-missing-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_pid(&path), None);
    }

    #[test]
    fn read_pid_returns_none_for_garbage_content() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-garbage-{}.pid", std::process::id()));
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
        let events_path = std::env::temp_dir().join(format!(
            "gitrun-gsr-crash-events-{}.jsonl",
            std::process::id()
        ));
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
