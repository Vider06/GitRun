//! External watchdog for the scheduler process.
//!
//! The watchdog monitors a PID file, records unexpected exits as critical
//! GSR events, and attempts a desktop notification with a terminal fallback.
//! PID-file reads fail explicitly on malformed, symlinked, non-regular or
//! hard-linked files so a corrupted file cannot be mistaken for clean shutdown.

use crate::events::{self, Severity};
use std::io::Read;
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
    // Do not cast malformed PID-file values into negative pid_t values:
    // kill(-1, 0) checks process-group-wide permissions and can falsely make
    // a corrupt watchdog target look alive forever.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
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
    let mut last_pid_file_error: Option<String> = None;

    while !should_stop() {
        match read_pid(&config.pid_file) {
            Ok(Some(pid)) => {
                last_pid_file_error = None;
                if process_is_alive(pid) {
                    last_known_pid = Some(pid);
                } else if last_known_pid == Some(pid) {
                    handle_crash(config, pid);
                    last_known_pid = None;
                }
            }
            Ok(None) => {
                last_pid_file_error = None;
                last_known_pid = None;
            }
            Err(error) => {
                let detail = error.to_string();
                if last_pid_file_error.as_deref() != Some(detail.as_str()) {
                    handle_pid_file_error(config, &detail);
                    last_pid_file_error = Some(detail);
                }
            }
        }
        std::thread::sleep(config.poll_interval);
    }
}

fn read_pid(path: &Path) -> Result<Option<u32>, std::io::Error> {
    validate_pid_parent(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "watchdog PID file must be a regular file without hard links",
            ));
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        return parse_pid_contents(&raw).map(Some);
    }
    #[cfg(not(unix))]
    {
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "watchdog PID file must be a regular file",
            ));
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        parse_pid_contents(&raw).map(Some)
    }
}

fn validate_pid_parent(path: &Path) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "watchdog PID parent must be a real directory, not a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "watchdog PID parent must not be group- or world-writable unless sticky",
            ));
        }
    }
    Ok(())
}

fn parse_pid_contents(raw: &str) -> Result<u32, std::io::Error> {
    let pid = raw.trim().parse::<u32>().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "watchdog PID file is malformed",
        )
    })?;
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "watchdog PID file contains an out-of-range PID",
        ));
    }
    Ok(pid)
}

fn handle_pid_file_error(config: &WatchConfig, detail: &str) {
    let message = format!(
        "cannot verify {} process because PID file {} is invalid: {detail}",
        config.watched_name,
        config.pid_file.display()
    );
    let event =
        crate::events::SecurityEvent::new("gsr-watchdog", Severity::Critical, message.clone());
    if let Err(error) = events::emit(&config.events_path, &event) {
        eprintln!(
            "gitrun-gsr: CRITICAL: {message} (additionally failed to write event log: {error})"
        );
    }
    show_error(&message);
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
    Command::new("/usr/bin/notify-send")
        .arg("GitRun — process crashed")
        .arg(message)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn invalid_pid_values_are_not_treated_as_live_processes() {
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(u32::MAX));
        assert!(!process_is_alive(i32::MAX as u32 + 1));
    }

    #[test]
    fn read_pid_parses_valid_file() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-test-{}.pid", std::process::id()));
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(read_pid(&path).unwrap(), Some(12345));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-missing-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_pid(&path).unwrap(), None);
    }

    #[test]
    fn read_pid_returns_none_for_garbage_content() {
        let path =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-garbage-{}.pid", std::process::id()));
        std::fs::write(&path, "not-a-pid").unwrap();
        assert!(read_pid(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_under_symlinked_parent_is_rejected() {
        use std::os::unix::fs::symlink;
        let base =
            std::env::temp_dir().join(format!("gitrun-gsr-pid-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("watch.pid"), "12345").unwrap();
        symlink(&real, &link).unwrap();
        assert!(read_pid(&link.join("watch.pid")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_symlink_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("gitrun-gsr-pid-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join("watch.pid");
        std::fs::write(&target, "12345").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_pid(&link).is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
