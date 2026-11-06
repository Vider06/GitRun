//! Shared updater settings, diagnostics and bounded local operation history.
//!
//! This module is owned by gitrun-management so the GUI, TUI and CLI use one
//! persistence contract. The update engine remains in gitrun-updater.

use super::{load_config, service_status};
use gitrun_platform::{ManagedService, ServiceManager};
use gitrun_updater::{build_plan, resolve_policy_update_for_target, UpdatePolicy};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SETTINGS_FILE: &str = "updater-settings.json";
const HISTORY_FILE: &str = "updater-history.json";
const HISTORY_LIMIT: usize = 25;
const DEFAULT_PUBLIC_KEY_HEX: &str =
    "11cc9eaf0147dada407ffd3497c2cebce17d195e21c06030c1e2e1dc688f41f1";
const DEFAULT_PUBLIC_KEY_ID: &str = "gitrun-release-ed25519-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicyChoice {
    SecurityOnly,
    SecurityFull,
    SecurityFullPre,
}

impl UpdatePolicyChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecurityOnly => "security_only",
            Self::SecurityFull => "security_full",
            Self::SecurityFullPre => "security_full_pre",
        }
    }

    fn engine_policy(self) -> UpdatePolicy {
        match self {
            Self::SecurityOnly => UpdatePolicy::SecurityOnly,
            Self::SecurityFull => UpdatePolicy::SecurityFull,
            Self::SecurityFullPre => UpdatePolicy::SecurityFullPre,
        }
    }
}

impl TryFrom<&str> for UpdatePolicyChoice {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "security_only" => Ok(Self::SecurityOnly),
            "security_full" => Ok(Self::SecurityFull),
            "security_full_pre" => Ok(Self::SecurityFullPre),
            _ => Err(format!("unsupported updater policy: {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdaterSettings {
    pub schema_version: u32,
    pub policy: UpdatePolicyChoice,
}

impl Default for UpdaterSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            // Stable installations receive security and consolidated full
            // releases by default; pre-releases require an explicit opt-in.
            policy: UpdatePolicyChoice::SecurityFull,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateOperation {
    pub timestamp: u64,
    pub from_version: String,
    pub to_version: Option<String>,
    pub release_type: Option<String>,
    pub result: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateServiceStatus {
    pub name: String,
    pub installed: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateLockStatus {
    pub status: String,
    pub owner_pid: Option<u32>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheckInfo {
    pub checked: bool,
    pub available: bool,
    pub selected_version: Option<String>,
    pub release_type: Option<String>,
    pub artifact: Option<String>,
    pub detail: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdaterInfo {
    pub version: String,
    pub package: String,
    pub architecture: String,
    pub abi: String,
    pub target: String,
    pub policy: String,
    pub policy_description: String,
    pub lock: UpdateLockStatus,
    pub services: Vec<UpdateServiceStatus>,
    pub recent_operations: Vec<UpdateOperation>,
    pub recent_errors: Vec<UpdateOperation>,
    pub check: Option<UpdateCheckInfo>,
}

#[cfg(target_os = "linux")]
fn validate_privileged_state_path(directory: &Path) -> Result<(), String> {
    use std::path::Component;
    if !directory.is_absolute() {
        return Err("privileged updater state directory must be absolute".into());
    }
    let mut prefix = PathBuf::from("/");
    let mut nearest_existing = prefix.clone();
    let mut missing_component = false;
    for component in directory.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => prefix.push(name),
            _ => return Err("privileged updater state path contains traversal components".into()),
        }
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => {
                if missing_component || metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!("unsafe updater state path component: {}", prefix.display()));
                }
                nearest_existing = prefix.clone();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing_component = true,
            Err(error) => return Err(format!("inspect updater state path {}: {error}", prefix.display())),
        }
    }
    gitrun_setup::validate_system_state_directory(&nearest_existing)
        .map_err(|error| format!("updater state directory is not trusted: {error}"))
}

fn state_dir() -> Result<PathBuf, String> {
    let config = load_config()?;
    let directory = PathBuf::from(config.state_dir);
    #[cfg(target_os = "linux")]
    if super::current_process_is_root() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        validate_privileged_state_path(&directory)?;
    }
    Ok(directory)
}

fn safe_state_dir(path: &Path, create: bool) -> Result<(), String> {
    if create {
        std::fs::create_dir_all(path)
            .map_err(|error| format!("create updater state directory: {error}"))?;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "inspect updater state directory {}: {error}",
                path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "refusing unsafe updater state directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "refusing non-regular updater state file: {}",
            path.display()
        )),
        Ok(_) => {
            let raw =
                std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
            serde_json::from_slice(&raw)
                .map_err(|error| format!("decode {}: {error}", path.display()))
        }
    }
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("updater state path has no parent")?;
    safe_state_dir(parent, true)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "refusing to replace unsafe updater state file: {}",
                path.display()
            ));
        }
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("encode updater state: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock unavailable: {error}"))?
        .as_nanos();
    let temporary = parent.join(format!(
        ".{}.{}-{nonce}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("updater"),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("create updater state transaction: {error}"))?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("commit updater state: {error}"))
}

pub fn read_settings_local() -> Result<UpdaterSettings, String> {
    let directory = state_dir()?;
    safe_state_dir(&directory, false)?;
    let settings: UpdaterSettings = read_json(&directory.join(SETTINGS_FILE))?;
    if settings.schema_version != 1 {
        return Err(format!(
            "unsupported updater settings schema version {}",
            settings.schema_version
        ));
    }
    Ok(settings)
}

pub fn save_settings_local(policy: &str) -> Result<UpdaterSettings, String> {
    let _write_guard = super::management_settings_write_guard()?;
    let policy = UpdatePolicyChoice::try_from(policy)?;
    let directory = state_dir()?;
    safe_state_dir(&directory, true)?;
    let settings = UpdaterSettings {
        schema_version: 1,
        policy,
    };
    atomic_write_json(&directory.join(SETTINGS_FILE), &settings)?;
    Ok(settings)
}

fn bounded_safe_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

pub fn record_operation(
    from_version: &str,
    to_version: Option<&str>,
    release_type: Option<&str>,
    result: &str,
    error: Option<&str>,
) -> Result<(), String> {
    let _write_guard = super::management_settings_write_guard()?;
    let directory = state_dir()?;
    safe_state_dir(&directory, true)?;
    let path = directory.join(HISTORY_FILE);
    let mut history: Vec<UpdateOperation> = read_json(&path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock unavailable: {error}"))?
        .as_secs();
    let safe_error = error.map(|value| value.chars().take(512).collect::<String>());
    history.push(UpdateOperation {
        timestamp,
        from_version: bounded_safe_text(from_version, 64),
        to_version: to_version.map(|value| bounded_safe_text(value, 64)),
        release_type: release_type
            .filter(|value| matches!(*value, "pre" | "full" | "security"))
            .map(|value| bounded_safe_text(value, 16)),
        result: bounded_safe_text(result, 32),
        error: safe_error.map(|value| bounded_safe_text(&value, 512)),
    });
    if history.len() > HISTORY_LIMIT {
        history.drain(..history.len() - HISTORY_LIMIT);
    }
    atomic_write_json(&path, &history)
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).is_dir()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn lock_status(directory: &Path) -> UpdateLockStatus {
    let lock = directory.join("gtuu.lock");
    let metadata = match std::fs::symlink_metadata(&lock) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return UpdateLockStatus {
                status: "idle".into(),
                owner_pid: None,
                detail: "No GTUU lifecycle operation is holding the shared lock.".into(),
            };
        }
        Err(error) => {
            return UpdateLockStatus {
                status: "unknown".into(),
                owner_pid: None,
                detail: format!("Cannot inspect the update lock: {error}"),
            }
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return UpdateLockStatus {
                status: "unsafe".into(),
                owner_pid: None,
                detail: "The GTUU lock path is not a regular directory.".into(),
            };
        }
        Ok(metadata) => metadata,
    };
    let _ = metadata;
    let pid_path = lock.join("pid");
    let pid = match std::fs::symlink_metadata(&pid_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
        }
        _ => None,
    };
    match pid {
        Some(pid) if process_is_alive(pid) => UpdateLockStatus {
            status: "active".into(), owner_pid: Some(pid),
            detail: "A GTUU reconciliation/update currently owns the shared lifecycle lock.".into(),
        },
        Some(pid) => UpdateLockStatus {
            status: "stale".into(), owner_pid: Some(pid),
            detail: "The recorded owner process is no longer running; the next lock acquisition will reclaim it safely.".into(),
        },
        None => UpdateLockStatus {
            status: "unknown".into(), owner_pid: None,
            detail: "The lock directory exists but has no readable owner PID.".into(),
        },
    }
}

fn installed_version() -> String {
    for path in [
        "/usr/share/gitrun/version.txt",
        "/usr/local/share/gitrun/version.txt",
        "version.txt",
    ] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    std::env::var("GITRUN_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Unknown".into())
}

fn package_type() -> String {
    if let Ok(value) = std::env::var("GITRUN_PACKAGE_TYPE") {
        if !value.trim().is_empty() {
            return value;
        }
    }
    if Path::new("/var/lib/dpkg/info/gitrun-server.list").is_file() {
        return "deb (gitrun-server)".into();
    }
    if Path::new("/var/lib/dpkg/info/gitrun.list").is_file() {
        return "deb (gitrun)".into();
    }
    if Path::new("/var/lib/apk/db/installed").is_file() {
        if std::fs::read_to_string("/var/lib/apk/db/installed")
            .ok()
            .is_some_and(|raw| {
                raw.lines()
                    .any(|line| matches!(line, "P:gitrun-server" | "P:gitrun"))
            })
        {
            return "apk".into();
        }
    }
    if Path::new("/var/lib/pacman/local").is_dir() {
        if std::fs::read_dir("/var/lib/pacman/local")
            .ok()
            .is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("gitrun-server-")
                        || entry.file_name().to_string_lossy().starts_with("gitrun-")
                })
            })
        {
            return "pkg.tar.zst".into();
        }
    }
    if Path::new("/var/db/xbps").is_dir() {
        if std::fs::read_dir("/var/db/xbps")
            .ok()
            .is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("gitrun-server-")
                        || entry.file_name().to_string_lossy().starts_with("gitrun-")
                })
            })
        {
            return "xbps".into();
        }
    }
    if Path::new("/var/lib/rpm").exists() || Path::new("/usr/lib/sysimage/rpm").exists() {
        if std::process::Command::new("/usr/bin/rpm")
            .args(["-q", "gitrun-server"])
            .output()
            .ok()
            .is_some_and(|output| output.status.success())
        {
            return "rpm (gitrun-server)".into();
        }
        if std::process::Command::new("/usr/bin/rpm")
            .args(["-q", "gitrun"])
            .output()
            .ok()
            .is_some_and(|output| output.status.success())
        {
            return "rpm (gitrun)".into();
        }
    }
    if Path::new("/usr/bin/gitrun").is_file() || Path::new("/usr/local/bin/gitrun").is_file() {
        return "manual/system archive".into();
    }
    "development/source".into()
}

fn package_abi(target: &str) -> String {
    if target.ends_with("-linux-gnu") {
        "gnu".into()
    } else if target.ends_with("-linux-musl") {
        "musl".into()
    } else if target.contains("windows") {
        "msvc".into()
    } else if target.contains("apple") {
        "darwin".into()
    } else {
        "unknown".into()
    }
}

fn service_snapshot() -> Vec<UpdateServiceStatus> {
    let (scheduler_installed, scheduler_active, _) = service_status(ManagedService::Scheduler);
    let manager = gitrun_platform::detect_service_manager();
    let (control_installed, control_active) = if manager == Some(ServiceManager::Systemd) {
        let installed = [
            "/etc/systemd/system/gitrun-control.service",
            "/usr/lib/systemd/system/gitrun-control.service",
            "/lib/systemd/system/gitrun-control.service",
        ]
        .iter()
        .any(|path| Path::new(path).is_file());
        let active = installed
            && gitrun_platform::trusted_service_manager_binary(ServiceManager::Systemd)
                .is_some_and(|program| {
                    std::process::Command::new(program)
                        .args(["is-active", "--quiet", "gitrun-control.service"])
                        .status()
                        .is_ok_and(|status| status.success())
                });
        (installed, active)
    } else {
        (false, false)
    };
    vec![
        UpdateServiceStatus {
            name: "Scheduler".into(),
            installed: scheduler_installed,
            active: scheduler_active,
        },
        UpdateServiceStatus {
            name: "Control Manager".into(),
            installed: control_installed,
            active: control_active,
        },
    ]
}

fn policy_description(policy: UpdatePolicyChoice) -> &'static str {
    match policy {
        UpdatePolicyChoice::SecurityOnly => "Security releases only",
        UpdatePolicyChoice::SecurityFull => "Security and consolidated full releases",
        UpdatePolicyChoice::SecurityFullPre => "Security, full and pre-releases",
    }
}

fn update_check(version: &str, policy: UpdatePolicyChoice, target: &str) -> UpdateCheckInfo {
    // Official update checks always use the built-in trust anchor. The release
    // manifest type is covered by its Ed25519 signature.
    std::env::set_var("GITRUN_UPDATE_PUBLIC_KEY_HEX", DEFAULT_PUBLIC_KEY_HEX);
    std::env::set_var("GITRUN_UPDATE_PUBLIC_KEY_ID", DEFAULT_PUBLIC_KEY_ID);
    std::env::set_var("GITRUN_UPDATE_SIGNATURE_REQUIRED", "true");
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    match resolve_policy_update_for_target(&repository, version, policy.engine_policy(), target) {
        Ok(Some(resolved)) => match build_plan(&resolved.manifest, version, target, &[]) {
            Ok(plan) => UpdateCheckInfo {
                checked: true,
                available: true,
                selected_version: Some(resolved.release.version),
                release_type: Some(resolved.release.release_type.as_str().into()),
                artifact: Some(plan.artifact),
                detail: format!("{} release {} is selected by policy {}.",
                    resolved.release.release_type.as_str(), resolved.release.version, policy.as_str()),
                error: None,
            },
            Err(error) => UpdateCheckInfo {
                checked: true, available: false,
                selected_version: Some(resolved.release.version),
                release_type: Some(resolved.release.release_type.as_str().into()),
                artifact: None,
                detail: "A newer release matched the policy but is not compatible with this installation.".into(),
                error: Some(error.to_string()),
            },
        },
        Ok(None) => UpdateCheckInfo {
            checked: true, available: false, selected_version: None, release_type: None,
            artifact: None,
            detail: format!("No newer release matches the {} policy.", policy.as_str()),
            error: None,
        },
        Err(error) => UpdateCheckInfo {
            checked: false, available: false, selected_version: None, release_type: None,
            artifact: None, detail: "Update check could not be completed.".into(),
            error: Some(error.to_string()),
        },
    }
}

pub fn info_local(check: bool) -> Result<UpdaterInfo, String> {
    let directory = state_dir()?;
    safe_state_dir(&directory, false)?;
    let settings = read_settings_local()?;
    let target = if std::env::consts::OS == "linux" {
        gitrun_platform::linux_target_triple()
            .unwrap_or_else(|_| format!("{}-unknown-linux-unknown", std::env::consts::ARCH))
    } else {
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    };
    let abi = package_abi(&target);
    let history: Vec<UpdateOperation> = read_json(&directory.join(HISTORY_FILE))?;
    let mut recent_errors = history
        .iter()
        .filter(|record| record.result == "failed")
        .cloned()
        .collect::<Vec<_>>();
    recent_errors.reverse();
    let mut recent_operations = history;
    recent_operations.reverse();
    let check_result = check.then(|| update_check(&installed_version(), settings.policy, &target));
    Ok(UpdaterInfo {
        version: installed_version(),
        package: package_type(),
        architecture: std::env::consts::ARCH.into(),
        abi,
        target,
        policy: settings.policy.as_str().into(),
        policy_description: policy_description(settings.policy).into(),
        lock: lock_status(&directory),
        services: service_snapshot(),
        recent_operations: recent_operations.into_iter().take(10).collect(),
        recent_errors: recent_errors.into_iter().take(5).collect(),
        check: check_result,
    })
}

pub fn current_settings_local() -> Result<UpdaterSettings, String> {
    read_settings_local()
}

pub fn local_settings_save(policy: &str) -> Result<UpdaterSettings, String> {
    save_settings_local(policy)
}

/// The CLI records completion at the transaction boundary, not from the UI.
pub fn record_cli_operation(
    from_version: &str,
    to_version: Option<&str>,
    release_type: Option<&str>,
    exit_code: i32,
) {
    let (result, error) = if exit_code == 0 {
        ("completed", None)
    } else {
        (
            "failed",
            Some("Update command returned a non-zero exit status"),
        )
    };
    let _ = record_operation(from_version, to_version, release_type, result, error);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_policy_has_stable_persisted_names_and_default() {
        assert_eq!(UpdaterSettings::default().policy.as_str(), "security_full");
        assert_eq!(
            UpdatePolicyChoice::try_from("security_only")
                .unwrap()
                .as_str(),
            "security_only"
        );
        assert_eq!(
            UpdatePolicyChoice::try_from("security_full_pre")
                .unwrap()
                .as_str(),
            "security_full_pre"
        );
        assert!(UpdatePolicyChoice::try_from("preview").is_err());
    }

    #[test]
    fn platform_abi_is_never_guessed_from_architecture() {
        assert_eq!(package_abi("x86_64-unknown-linux-gnu"), "gnu");
        assert_eq!(package_abi("aarch64-unknown-linux-musl"), "musl");
        assert_eq!(package_abi("x86_64-unknown-linux-unknown"), "unknown");
    }
}
