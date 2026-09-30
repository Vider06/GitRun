pub mod ui;

use gitrun_core::{Config, StateStore};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SYSTEMD_UNIT: &str = include_str!("../../../systemd/gitrun.service");

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryIssue {
    pub code: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub repairable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct UpdateStatus {
    pub checked: bool,
    pub available: bool,
    pub current_version: String,
    pub target_version: Option<String>,
    pub applied: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryReport {
    pub healthy: bool,
    pub version: String,
    pub config_path: Option<String>,
    pub gitrun_binary: Option<String>,
    pub config_ok: bool,
    pub state_ok: bool,
    pub docker_ok: bool,
    pub service_unit_ok: bool,
    pub update: UpdateStatus,
    pub issues: Vec<RecoveryIssue>,
}

impl RecoveryReport {
    pub fn has_critical(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == Severity::Critical)
    }
}

pub fn inspect() -> RecoveryReport {
    let version = current_version();
    let config_path = configured_config_file();
    let mut issues = Vec::new();

    let config = match config_path.as_deref() {
        Some(path) => match Config::from_env_file(path) {
            Ok(config) => Some(config),
            Err(error) => {
                issues.push(RecoveryIssue {
                    code: "config-invalid".into(),
                    severity: Severity::Critical,
                    title: "Configuration is invalid".into(),
                    detail: error.to_string(),
                    repairable: true,
                });
                None
            }
        },
        None => None,
    };

    if config_path.is_none() {
        issues.push(RecoveryIssue {
            code: "config-missing".into(),
            severity: Severity::Warning,
            title: "GitRun is not configured yet".into(),
            detail: "No persistent gitrun.env was found.".into(),
            repairable: true,
        });
    }

    let binary = find_gitrun_binary();
    if binary.is_none() {
        issues.push(RecoveryIssue {
            code: "binary-missing".into(),
            severity: Severity::Critical,
            title: "GitRun executable is missing".into(),
            detail: "The installed GitRun CLI could not be found.".into(),
            repairable: true,
        });
    } else if config.is_some() {
        let path = binary.as_deref().expect("binary already checked");
        if let Err(error) =
            gitrun_updater::health_check_installed_binary(path, config_path.as_deref())
        {
            issues.push(RecoveryIssue {
                code: "binary-unhealthy".into(),
                severity: Severity::Critical,
                title: "GitRun executable failed its health check".into(),
                detail: error.to_string(),
                repairable: true,
            });
        }
    }

    let state_dir = config
        .as_ref()
        .map(|value| PathBuf::from(&value.state_dir))
        .unwrap_or_else(|| PathBuf::from("/var/lib/gitrun"));
    let store = StateStore::new(&state_dir);

    let state_ok = match store.read_health() {
        Ok(Some(report)) if !report.healthy => {
            issues.push(RecoveryIssue {
                code: "previous-health-failed".into(),
                severity: Severity::Warning,
                title: "GitRun recorded an unhealthy previous startup".into(),
                detail: report.message,
                repairable: false,
            });
            true
        }
        Ok(Some(_)) | Ok(None) => true,
        Err(error) => {
            issues.push(RecoveryIssue {
                code: "health-corrupt".into(),
                severity: Severity::Critical,
                title: "Health state cannot be read".into(),
                detail: error.to_string(),
                repairable: true,
            });
            false
        }
    };

    match store.read_last_crash() {
        Ok(Some(crash)) if !crash.trim().is_empty() => {
            issues.push(RecoveryIssue {
                code: "previous-crash".into(),
                severity: if state_ok {
                    Severity::Info
                } else {
                    Severity::Warning
                },
                title: "A previous GitRun failure was recorded".into(),
                detail: crash.trim().to_owned(),
                repairable: false,
            });
        }
        Ok(Some(_)) | Ok(None) => {}
        Err(error) => {
            issues.push(RecoveryIssue {
                code: "crash-log-unreadable".into(),
                severity: Severity::Warning,
                title: "Previous crash record cannot be read".into(),
                detail: error.to_string(),
                repairable: false,
            });
        }
    }

    let docker_ok = command_succeeds("docker", &["info"]);
    if !docker_ok {
        issues.push(RecoveryIssue {
            code: "docker-unavailable".into(),
            severity: Severity::Warning,
            title: "Docker is unavailable".into(),
            detail: "Runner management will not work until Docker is reachable.".into(),
            repairable: false,
        });
    }

    let service_unit_ok = service_unit_is_valid();
    if !service_unit_ok {
        issues.push(RecoveryIssue {
            code: "service-invalid".into(),
            severity: Severity::Warning,
            title: "GitRun system service is missing or stale".into(),
            detail: "The installed service does not use the recovery launcher.".into(),
            repairable: true,
        });
    }

    if let Some(config) = &config {
        if config.repositories.is_empty() {
            issues.push(RecoveryIssue {
                code: "repositories-empty".into(),
                severity: Severity::Warning,
                title: "No repositories are configured".into(),
                detail: "The dashboard can start, but the scheduler has nothing to reconcile."
                    .into(),
                repairable: true,
            });
        }
    }

    RecoveryReport {
        healthy: !issues
            .iter()
            .any(|issue| issue.severity == Severity::Critical),
        version,
        config_path: config_path.map(|path| path.display().to_string()),
        gitrun_binary: binary.map(|path| path.display().to_string()),
        config_ok: config.is_some(),
        state_ok,
        docker_ok,
        service_unit_ok,
        update: UpdateStatus::default(),
        issues,
    }
}

pub fn startup(target: StartupTarget) -> Result<RecoveryReport, String> {
    let mut report = inspect();

    if target == StartupTarget::Scheduler {
        if !report.config_ok {
            report.issues.push(RecoveryIssue {
                code: "scheduler-config-required".into(),
                severity: Severity::Critical,
                title: "Scheduler cannot start without valid configuration".into(),
                detail: "Run the GitRun setup flow first.".into(),
                repairable: true,
            });
        }
        if !report.docker_ok {
            report.issues.push(RecoveryIssue {
                code: "scheduler-docker-required".into(),
                severity: Severity::Critical,
                title: "Scheduler cannot start without Docker".into(),
                detail: "Docker must be reachable before runner reconciliation can begin.".into(),
                repairable: false,
            });
        }
    }

    if report.config_ok && report.gitrun_binary.is_some() {
        match run_gtuu() {
            Ok(gtuu) => {
                let target_changed = gtuu
                    .target_version
                    .as_deref()
                    .map(|value| value != report.version)
                    .unwrap_or(false);
                let applied = gtuu.gitrun_updated;
                report.update = UpdateStatus {
                    checked: true,
                    available: target_changed && !applied,
                    current_version: if applied {
                        gtuu.target_version
                            .clone()
                            .unwrap_or(gtuu.current_version.clone())
                    } else {
                        gtuu.current_version.clone()
                    },
                    target_version: gtuu.target_version.clone(),
                    applied,
                    error: gtuu
                        .gitrun_update_error
                        .clone()
                        .or(gtuu.runner_image_error.clone())
                        .or(gtuu.containers_error.clone()),
                };

                if let Some(error) = gtuu.gitrun_update_error {
                    report.issues.push(RecoveryIssue {
                        code: "gtuu-gitrun".into(),
                        severity: Severity::Warning,
                        title: "GitRun update check failed".into(),
                        detail: error,
                        repairable: true,
                    });
                }
                if let Some(error) = gtuu.runner_image_error {
                    report.issues.push(RecoveryIssue {
                        code: "gtuu-runner-image".into(),
                        severity: Severity::Warning,
                        title: "Runner image update failed".into(),
                        detail: error,
                        repairable: true,
                    });
                }
                if let Some(error) = gtuu.containers_error {
                    report.issues.push(RecoveryIssue {
                        code: "gtuu-containers".into(),
                        severity: Severity::Warning,
                        title: "GTUU could not reconcile runner containers".into(),
                        detail: error,
                        repairable: true,
                    });
                }
            }
            Err(error) => {
                report.update.checked = true;
                report.update.error = Some(error.clone());
                report.issues.push(RecoveryIssue {
                    code: "gtuu-failed".into(),
                    severity: Severity::Warning,
                    title: "GTUU could not complete".into(),
                    detail: error,
                    repairable: true,
                });
            }
        }
    }

    report.healthy = !report.has_critical();
    Ok(report)
}

pub fn run_gtuu() -> Result<gitrun_scheduler::GtuuStartupReport, String> {
    gitrun_scheduler::gtuu_startup::run_gtuu_startup_once().map_err(|error| error.to_string())
}

pub fn find_gitrun_binary() -> Option<PathBuf> {
    let candidates = [
        std::env::var("GITRUN_BINARY_PATH").ok().map(PathBuf::from),
        Some(PathBuf::from("/usr/local/bin/gitrun")),
        Some(PathBuf::from("/usr/bin/gitrun")),
        std::env::current_exe().ok().and_then(|path| {
            path.parent().map(|dir| {
                dir.join(if cfg!(windows) {
                    "gitrun.exe"
                } else {
                    "gitrun"
                })
            })
        }),
    ];
    candidates.into_iter().flatten().find(|path| path.is_file())
}

pub fn configured_config_file() -> Option<PathBuf> {
    std::env::var("GITRUN_CONFIG_FILE")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let path = PathBuf::from("/etc/gitrun/gitrun.env");
            path.is_file().then_some(path)
        })
        .or_else(|| {
            let path = PathBuf::from("config/gitrun.env");
            path.is_file().then_some(path)
        })
}

pub fn current_version() -> String {
    std::env::var("GITRUN_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            fs::read_to_string("/usr/share/gitrun/version.txt")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .or_else(|| {
            fs::read_to_string("version.txt")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "0.0.0".into())
}

pub fn dashboard_binary() -> Option<PathBuf> {
    let candidates = [
        std::env::var("GITRUN_DASHBOARD_BINARY")
            .ok()
            .map(PathBuf::from),
        Some(PathBuf::from("/usr/bin/gitrun-dashboard-tauri")),
        Some(PathBuf::from("/usr/local/bin/gitrun-dashboard-tauri")),
        std::env::current_exe().ok().and_then(|path| {
            path.parent().map(|dir| {
                dir.join(if cfg!(windows) {
                    "gitrun-dashboard-tauri.exe"
                } else {
                    "gitrun-dashboard-tauri"
                })
            })
        }),
    ];
    candidates.into_iter().flatten().find(|path| path.is_file())
}

pub fn service_unit_is_valid() -> bool {
    let path = Path::new("/etc/systemd/system/gitrun.service");
    fs::read_to_string(path)
        .map(|content| content.contains("ExecStart=/usr/local/bin/gitrun-recovery start scheduler"))
        .unwrap_or(false)
}

pub fn repair_service_unit() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("system service repair currently targets Linux".into());
    }
    if !is_root() {
        return Err("service repair requires root privileges".into());
    }

    let path = Path::new("/etc/systemd/system/gitrun.service");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let temp = path.with_file_name(format!(
        "gitrun.service.tmp.{}.{}",
        std::process::id(),
        nonce
    ));
    fs::write(&temp, SYSTEMD_UNIT).map_err(|error| error.to_string())?;
    set_file_mode(&temp, 0o644)?;
    fs::rename(&temp, path).map_err(|error| error.to_string())?;

    run_systemctl("daemon-reload", "gitrun.service")?;
    run_systemctl("enable", "gitrun.service")?;
    run_systemctl("restart", "gitrun.service")
}

pub fn is_root_for_ui() -> bool {
    is_root()
}

pub fn restart_service() -> Result<(), String> {
    if is_root() {
        return repair_service_unit();
    }

    let recovery = std::env::var("GITRUN_RECOVERY_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/gitrun-recovery"));
    let status = Command::new("pkexec")
        .arg(recovery)
        .arg("repair-service")
        .status()
        .map_err(|error| format!("unable to request privileged service repair: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("privileged service repair exited with {status}"))
    }
}

fn run_systemctl(action: &str, unit: &str) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args([action, unit])
        .output()
        .map_err(|error| format!("unable to start systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn is_root() -> bool {
    command_succeeds("id", &["-u", "0"])
}

pub fn launch_dashboard() -> Result<std::process::ExitStatus, String> {
    let binary = dashboard_binary().ok_or("GitRun Tauri dashboard executable was not found")?;
    Command::new(binary)
        .status()
        .map_err(|error| format!("unable to launch GitRun dashboard: {error}"))
}

pub fn launch_scheduler() -> Result<std::process::ExitStatus, String> {
    let binary = find_gitrun_binary().ok_or("GitRun CLI executable was not found")?;
    Command::new(binary)
        .args(["scheduler"])
        .status()
        .map_err(|error| format!("unable to launch GitRun scheduler: {error}"))
}

pub fn mark_startup_healthy() -> Result<(), String> {
    let config_path = configured_config_file();
    let state_dir = config_path
        .as_deref()
        .and_then(|path| Config::from_env_file(path).ok())
        .map(|config| PathBuf::from(config.state_dir))
        .unwrap_or_else(|| PathBuf::from("/var/lib/gitrun"));
    StateStore::new(state_dir)
        .write_health(true, "GitRun startup preflight passed")
        .map_err(|error| error.to_string())
}

pub fn record_failure(message: &str) -> Result<(), String> {
    let config_path = configured_config_file();
    let state_dir = config_path
        .as_deref()
        .and_then(|path| Config::from_env_file(path).ok())
        .map(|config| PathBuf::from(config.state_dir))
        .unwrap_or_else(|| PathBuf::from("/var/lib/gitrun"));
    let store = StateStore::new(state_dir);
    store
        .write_health(false, message)
        .map_err(|error| error.to_string())?;
    store
        .record_crash(message)
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupTarget {
    Scheduler,
    Dashboard,
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_report_is_detected() {
        let report = RecoveryReport {
            healthy: false,
            version: "0.4.0".into(),
            config_path: None,
            gitrun_binary: None,
            config_ok: false,
            state_ok: false,
            docker_ok: false,
            service_unit_ok: false,
            update: UpdateStatus::default(),
            issues: vec![RecoveryIssue {
                code: "test".into(),
                severity: Severity::Critical,
                title: "critical".into(),
                detail: "test".into(),
                repairable: true,
            }],
        };
        assert!(report.has_critical());
    }

    #[test]
    fn warning_only_report_is_not_critical() {
        let report = RecoveryReport {
            healthy: true,
            version: "0.4.0".into(),
            config_path: None,
            gitrun_binary: None,
            config_ok: true,
            state_ok: true,
            docker_ok: true,
            service_unit_ok: true,
            update: UpdateStatus::default(),
            issues: vec![RecoveryIssue {
                code: "test".into(),
                severity: Severity::Warning,
                title: "warning".into(),
                detail: "test".into(),
                repairable: true,
            }],
        };
        assert!(!report.has_critical());
    }
}
