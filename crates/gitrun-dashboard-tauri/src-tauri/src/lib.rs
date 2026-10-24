//! GitRun dashboard backend (Tauri).
//! with a native Rust Tauri application. The backend exposes `#[tauri::command]`s that a web frontend (`../dist`)
//! calls via `invoke(...)`. No HTTP server involved — Tauri's IPC bridges
//! JS calls directly into these Rust functions in the same process.
//!
//! Every command here reads live data from GitRun's real crates
//! (`gitrun-core::Config`, `gitrun-vault::Vault`, `gitrun-gsr::events`) —
//! nothing in this file is mocked or hardcoded sample data, by design: a
//! dashboard showing fake data would be actively misleading for an
//! infrastructure tool.

use gitrun_core::{Config, GitRunApi, GitRunSettings};
use gitrun_scheduler::gsr_bridge::VaultToGsrBridge;
use gitrun_setup::BootstrapAuth;
use gitrun_smart_scheduler::{KittyDashboardStatus, KittyRuntime, KittySettings};
use gitrun_vault::{Scope, Vault};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

mod control_manager;
pub use control_manager::{
    enabled as control_manager_enabled, request as control_manager_request,
    serve as run_control_manager,
};

/// Serializes dashboard-side writes to the shared config/settings files.
/// The snapshot check alone is vulnerable to two Tauri commands checking the
/// same old state before either one writes it.
static DASHBOARD_SETTINGS_WRITE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn dashboard_settings_write_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    DASHBOARD_SETTINGS_WRITE_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .map_err(|_| "dashboard settings write lock is poisoned; refusing to save".to_owned())
}

fn resolve_config_path(explicit: Option<&std::ffi::OsStr>) -> Result<Option<PathBuf>, String> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(format!(
                "GITRUN_CONFIG_FILE points to a missing configuration file: {}",
                path.display()
            ));
        }
        return Ok(Some(path));
    }

    Ok([
        PathBuf::from("/etc/gitrun/gitrun.env"),
        PathBuf::from("config/gitrun.env"),
    ]
    .into_iter()
    .find(|path| path.is_file()))
}

fn load_config() -> Result<Config, String> {
    // System configuration contains credentials and stays root-owned 0600.
    // Desktop processes read it only through the persistent, allowlisted IPC
    // service. The manager itself runs as root and follows the local path below.
    if control_manager_enabled() && !current_process_is_root() {
        let value = control_manager_request("read", "config", serde_json::json!({}))?;
        return serde_json::from_value(value)
            .map_err(|error| format!("decode authorized GitRun configuration: {error}"));
    }
    let explicit = std::env::var_os("GITRUN_CONFIG_FILE");
    let path = resolve_config_path(explicit.as_deref())?;
    match path {
        Some(path) => Config::from_env_file(path),
        None => Config::from_env(),
    }
    .map_err(|e| e.to_string())
}

fn configured_config_path() -> Option<PathBuf> {
    resolve_config_path(std::env::var_os("GITRUN_CONFIG_FILE").as_deref())
        .ok()
        .flatten()
}

fn config_path_for_write() -> Result<PathBuf, String> {
    let explicit = std::env::var_os("GITRUN_CONFIG_FILE");
    resolve_config_path(explicit.as_deref())?
        .or_else(|| {
            // Desktop-launched dashboards don't inherit the scheduler service's
            // environment. Resolve the standard installation path from disk.
            let system_path = PathBuf::from("/etc/gitrun/gitrun.env");
            system_path.is_file().then_some(system_path)
        })
        .or_else(|| {
            let local_path = PathBuf::from("config/gitrun.env");
            local_path.is_file().then_some(local_path)
        })
        .ok_or_else(|| {
            "No existing GitRun configuration file found. Run GitRun setup before saving settings."
                .to_owned()
        })
}

fn trusted_system_cli_path() -> Option<PathBuf> {
    [
        PathBuf::from("/usr/bin/gitrun"),
        PathBuf::from("/usr/local/bin/gitrun"),
    ]
    .into_iter()
    .find_map(|path| trusted_privileged_binary(&path))
}

#[cfg(target_os = "linux")]
fn authorized_cli_command(cli: &Path) -> Result<std::process::Command, String> {
    let mut command = if current_process_is_root() {
        std::process::Command::new(cli)
    } else {
        let pkexec = pkexec_path().ok_or(
            "trusted pkexec was not found; this protected operation cannot continue safely",
        )?;
        let mut command = std::process::Command::new(pkexec);
        command.arg(cli);
        command
    };
    command.arg("--no-cat");
    Ok(command)
}

#[cfg(target_os = "linux")]
fn persist_config_as_root(config: &Config) -> Result<(), String> {
    if control_manager_enabled() && !current_process_is_root() {
        control_manager_request(
            "write",
            "config_save",
            serde_json::json!({ "updated": config }),
        )?;
        return Ok(());
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; system settings cannot be saved safely.",
    )?;
    let payload = serde_json::to_vec(config).map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-save-config-{}-{unique}.json",
        std::process::id()
    ));

    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(&payload)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;

        let status = authorized_cli_command(&cli)?
            .arg("--save-config-root")
            .arg(&path)
            .status()
            .map_err(|error| format!("unable to request administrator authorization: {error}"))?;
        if !status.success() {
            return Err(format!(
                "administrator-authorized settings save failed with status {status}"
            ));
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "linux"))]
fn persist_config_as_root(_config: &Config) -> Result<(), String> {
    Err("administrator-authorized system settings persistence is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn persist_gitrun_settings_as_root(settings: &GitRunSettings) -> Result<(), String> {
    if control_manager_enabled() && !current_process_is_root() {
        control_manager_request(
            "write",
            "gitrun_settings_save",
            serde_json::json!({
                "settings": settings,
                "confirm_socket_opt_out": true
            }),
        )?;
        return Ok(());
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; security settings cannot be saved safely.",
    )?;
    let payload = serde_json::to_vec(settings).map_err(|error| error.to_string())?;
    if payload.len() > 1024 * 1024 {
        return Err("GitRun security settings exceed the 1 MiB safety limit".into());
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-save-security-settings-{}-{unique}.json",
        std::process::id()
    ));

    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(&payload)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;

        let status = authorized_cli_command(&cli)?
            .arg("--save-gitrun-settings-root")
            .arg(&path)
            .status()
            .map_err(|error| format!("unable to request administrator authorization: {error}"))?;
        if !status.success() {
            return Err(format!(
                "administrator-authorized security-settings save failed with status {status}"
            ));
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "linux"))]
fn persist_gitrun_settings_as_root(_settings: &GitRunSettings) -> Result<(), String> {
    Err("administrator-authorized security-settings persistence is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn persist_logic_rules_as_root(
    rules: &[gitrun_scheduler::logic_containers::LogicRule],
) -> Result<(), String> {
    if control_manager_enabled() && !current_process_is_root() {
        control_manager_request(
            "write",
            "logic_rules_save",
            serde_json::json!({ "rules": rules }),
        )?;
        return Ok(());
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    gitrun_scheduler::logic_containers::validate_rules(rules).map_err(|error| error.to_string())?;
    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; Logic Containers rules cannot be saved safely.",
    )?;
    let payload = serde_json::to_vec(rules).map_err(|error| error.to_string())?;
    if payload.len() > 1024 * 1024 {
        return Err("Logic Containers rules exceed the 1 MiB safety limit".into());
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-save-logic-rules-{}-{unique}.json",
        std::process::id()
    ));

    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(&payload)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);

        let status = authorized_cli_command(&cli)?
            .arg("--save-logic-rules-root")
            .arg(&path)
            .status()
            .map_err(|error| format!("unable to request administrator authorization: {error}"))?;
        if !status.success() {
            return Err(format!(
                "administrator-authorized Logic Containers save failed with status {status}"
            ));
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "linux"))]
fn persist_logic_rules_as_root(
    _rules: &[gitrun_scheduler::logic_containers::LogicRule],
) -> Result<(), String> {
    Err("administrator-authorized Logic Containers persistence is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn persist_vm_configs_as_root(configs: &[gitrun_scheduler::vm::VmConfig]) -> Result<(), String> {
    if control_manager_enabled() && !current_process_is_root() {
        control_manager_request(
            "write",
            "vm_configs_replace",
            serde_json::json!({ "configs": configs }),
        )?;
        return Ok(());
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; VM configuration cannot be saved safely.",
    )?;
    let payload = serde_json::to_vec(configs).map_err(|error| error.to_string())?;
    if payload.len() > 1024 * 1024 {
        return Err("VM configuration exceeds the 1 MiB safety limit".into());
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-save-vm-configs-{}-{unique}.json",
        std::process::id()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(&payload)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        let status = authorized_cli_command(&cli)?
            .arg("--save-vm-configs-root")
            .arg(&path)
            .status()
            .map_err(|error| format!("unable to request administrator authorization: {error}"))?;
        if !status.success() {
            return Err(format!(
                "administrator-authorized VM configuration save failed with status {status}"
            ));
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "linux"))]
fn persist_vm_configs_as_root(_configs: &[gitrun_scheduler::vm::VmConfig]) -> Result<(), String> {
    Err("administrator-authorized VM configuration persistence is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn respond_hypervisor_decision_as_root(
    vm_name: &str,
    choice: gitrun_core::hypervisor_decision::DecisionChoice,
) -> Result<(), String> {
    if vm_name.trim().is_empty()
        || vm_name.chars().any(char::is_control)
        || vm_name.contains('/')
        || vm_name.contains('\\')
        || vm_name.len() > 128
    {
        return Err("VM decision name is invalid".into());
    }
    let choice_arg = match choice {
        gitrun_core::hypervisor_decision::DecisionChoice::RetryKvm => "retry_kvm",
        gitrun_core::hypervisor_decision::DecisionChoice::UseVirtualBox => "use_virtual_box",
    };
    let _ = run_privileged_cli_helper(
        "--respond-hypervisor-decision-root",
        &["--".to_owned(), vm_name.to_owned(), choice_arg.to_owned()],
    )?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn respond_hypervisor_decision_as_root(
    _vm_name: &str,
    _choice: gitrun_core::hypervisor_decision::DecisionChoice,
) -> Result<(), String> {
    Err("administrator-authorized hypervisor decisions are Linux-only".into())
}

#[cfg(target_os = "linux")]
fn persist_kitty_settings_as_root(settings: &KittySettings) -> Result<(), String> {
    if control_manager_enabled() && !current_process_is_root() {
        control_manager_request(
            "write",
            "kitty_save",
            serde_json::json!({ "settings": settings }),
        )?;
        return Ok(());
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; Kitty settings cannot be saved safely.",
    )?;
    let payload = serde_json::to_vec(settings).map_err(|error| error.to_string())?;
    if payload.len() > 64 * 1024 {
        return Err("Kitty settings exceed the 64 KiB safety limit".into());
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-save-kitty-settings-{}-{unique}.json",
        std::process::id()
    ));

    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(&payload)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);

        let status = authorized_cli_command(&cli)?
            .arg("--save-kitty-settings-root")
            .arg(&path)
            .status()
            .map_err(|error| format!("unable to request administrator authorization: {error}"))?;
        if !status.success() {
            return Err(format!(
                "administrator-authorized Kitty settings save failed with status {status}"
            ));
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "linux"))]
fn persist_kitty_settings_as_root(_settings: &KittySettings) -> Result<(), String> {
    Err("administrator-authorized Kitty settings persistence is Linux-only".into())
}

/// Persist security policy through the privileged helper for system installs.
/// The state directory is root-owned so user-level atomic rename is intentionally
/// unavailable; developer installations retain their configurable local path.
fn persist_gitrun_settings(settings: &GitRunSettings, path: &Path) -> Result<(), String> {
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        persist_gitrun_settings_as_root(settings)
    } else {
        settings.save(path).map_err(|error| error.to_string())
    }
}

/// Persists only known settings. On a system installation, the configuration is
/// root-owned; a desktop-launched dashboard must request authorization rather
/// than relying on the service's GITRUN_CONFIG_FILE environment.
fn persist_dashboard_config_unlocked(config: &Config) -> Result<(), String> {
    config.validate().map_err(|error| error.to_string())?;
    let path = config_path_for_write()?;

    match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => {
            drop(file);
            gitrun_setup::update_env_file(&path, config).map_err(|error| error.to_string())
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && path == Path::new("/etc/gitrun/gitrun.env") =>
        {
            persist_config_as_root(config)
        }
        Err(error) => Err(format!(
            "cannot write GitRun configuration {}: {error}",
            path.display()
        )),
    }
}

fn persist_dashboard_config(config: &Config) -> Result<(), String> {
    let _write_guard = dashboard_settings_write_guard()?;
    persist_dashboard_config_unlocked(config)
}

#[tauri::command]
fn is_first_run() -> bool {
    matches!(
        gitrun_setup::installation_state(),
        gitrun_setup::InstallationState::FirstRun
    )
}

fn trusted_privileged_binary(path: &std::path::Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }

    let canonical = path.canonicalize().ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = std::fs::symlink_metadata(&canonical).ok()?;
        if metadata.uid() != 0 || !trusted_executable_mode(metadata.permissions().mode() & 0o777) {
            return None;
        }

        let mut current = canonical.parent();
        while let Some(directory) = current {
            let metadata = std::fs::symlink_metadata(directory).ok()?;
            if !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.permissions().mode() & 0o022 != 0
            {
                return None;
            }
            if directory == std::path::Path::new("/") {
                break;
            }
            current = directory.parent();
        }
    }

    Some(canonical)
}

fn dashboard_cli_path(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    // Prefer the current package-managed or server-installed CLI. Reinstall
    // replaces these paths; a bundled resource copy could otherwise be stale.
    #[cfg(unix)]
    {
        candidates.push(PathBuf::from("/usr/bin/gitrun"));
        candidates.push(PathBuf::from("/usr/local/bin/gitrun"));
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join(if cfg!(windows) {
            "gitrun.exe"
        } else {
            "gitrun"
        }));
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(if cfg!(windows) {
                "gitrun.exe"
            } else {
                "gitrun"
            }));
        }
    }

    #[cfg(windows)]
    {
        candidates.push(PathBuf::from(r"C:\Program Files\GitRun\gitrun.exe"));
    }

    candidates
        .into_iter()
        .find_map(|path| trusted_privileged_binary(&path))
}

fn pkexec_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        [
            PathBuf::from("/usr/bin/pkexec"),
            PathBuf::from("/bin/pkexec"),
        ]
        .into_iter()
        .find_map(|path| trusted_privileged_binary(&path))
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn valid_setup_repository(value: &str) -> bool {
    let Some((owner, repository)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repository.is_empty()
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && repository
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn process_has_name(pid: u32, expected_name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|name| name.trim() == expected_name)
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, expected_name);
        false
    }
}

fn process_name_running(expected_name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|value| value.parse::<u32>().ok()) else {
                return false;
            };
            if process_has_name(pid, expected_name) {
                return true;
            }
            // GSR is launched as a dedicated process of the single packaged
            // GitRun executable to keep distribution single-binary.
            expected_name == "gitrun-gsr"
                && std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                    .map(|raw| raw.split('\0').any(|argument| argument == "gsr-watchdog"))
                    .unwrap_or(false)
        })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = expected_name;
        false
    }
}

fn recent_event_cutoff() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().saturating_sub(24 * 60 * 60))
        .unwrap_or(0)
}

fn validate_setup_value(value: &str, key: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{key} is required"));
    }
    if value.chars().any(|c| c == '\n' || c == '\r') {
        return Err(format!("{key} must not contain newlines"));
    }
    Ok(value.to_owned())
}

fn validate_numeric_setup_id(value: &str, key: &str) -> Result<String, String> {
    let value = validate_setup_value(value, key)?;
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{key} must contain only digits"));
    }
    Ok(value)
}

fn validate_private_key_path(value: &str) -> Result<String, String> {
    let path = validate_setup_value(value, "private key path")?;
    let path_buf = PathBuf::from(&path);
    secure_private_key_file(&path_buf)?;
    // Resolve relative paths before elevation: pkexec/sudo may start the
    // privileged helper in a different working directory.
    let canonical = path_buf.canonicalize().map_err(|error| {
        format!(
            "unable to resolve GitHub App private key {}: {error}",
            path_buf.display()
        )
    })?;
    secure_private_key_file(&canonical)?;
    Ok(canonical.to_string_lossy().into_owned())
}

/// Harden a user-supplied GitHub App private key before the privileged setup
/// process receives its path. On Unix this deliberately normalizes the file
/// to 0600 rather than merely rejecting common download modes such as 0644 or
/// 0664: the operator already owns the file, and root can still read a 0600
/// file when the setup is elevated through pkexec.
fn secure_private_key_file(path: &std::path::Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "unable to inspect GitHub App private key {}: {e}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err("GitHub App private key path must not be a symbolic link".into());
    }
    if !metadata.is_file() {
        return Err(format!(
            "GitHub App private key path is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > 1024 * 1024 {
        return Err("GitHub App private key exceeds the 1 MiB safety limit".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            format!(
                "unable to secure GitHub App private key {} to mode 0600: {e}",
                path.display()
            )
        })?;

        let metadata = std::fs::symlink_metadata(path).map_err(|e| {
            format!(
                "unable to re-check GitHub App private key {} after chmod: {e}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(
                "GitHub App private key path changed or is no longer a regular file".into(),
            );
        }

        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(format!(
                "GitHub App private key could not be secured to mode 0600 (actual mode {mode:o})"
            ));
        }

        std::fs::File::open(path).map_err(|e| {
            format!(
                "GitHub App private key {} is not readable after securing it to 0600: {e}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn trusted_executable_mode(mode: u32) -> bool {
    mode & 0o022 == 0 && mode & 0o111 != 0
}

#[tauri::command]
fn secure_private_key(private_key_path: String) -> Result<String, String> {
    let path = validate_setup_value(&private_key_path, "private key path")?;
    let path_buf = PathBuf::from(&path);
    secure_private_key_file(&path_buf)?;
    Ok(path)
}

fn build_first_setup_auth(
    auth_mode: &str,
    token: &str,
    app_id: &str,
    installation_id: &str,
    private_key_path: &str,
) -> Result<BootstrapAuth, String> {
    match auth_mode.trim().to_ascii_lowercase().as_str() {
        "pat" => Ok(BootstrapAuth::Pat(validate_setup_value(
            token,
            "GitHub token",
        )?)),
        "app" => Ok(BootstrapAuth::GitHubApp {
            app_id: validate_numeric_setup_id(app_id, "GitHub App ID")?,
            installation_id: validate_numeric_setup_id(
                installation_id,
                "GitHub App installation ID",
            )?,
            private_key_path: validate_private_key_path(private_key_path)?,
        }),
        other => Err(format!("unsupported GitHub authentication mode: {other}")),
    }
}

fn write_setup_request(path: &std::path::Path, payload: &str) -> Result<(), String> {
    let mut created_file = false;
    let result = (|| -> Result<(), String> {
        use std::fs::OpenOptions;
        use std::io::Write;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(path).map_err(|e| e.to_string())?;
        created_file = true;
        file.write_all(payload.as_bytes())
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    })();

    match result {
        Ok(()) => Ok(()),
        Err(write_error) if !created_file => Err(write_error),
        Err(write_error) => match std::fs::remove_file(path) {
            Ok(()) => Err(write_error),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                Err(write_error)
            }
            Err(cleanup_error) => Err(format!(
                "{write_error}; additionally unable to remove the partial setup request file: {cleanup_error}"
            )),
        },
    }
}

#[derive(Debug, Clone, Serialize)]
struct FirstSetupEvent {
    phase: u8,
    total: u8,
    message: String,
    stream: String,
    done: bool,
    success: bool,
}

const FIRST_SETUP_EVENT: &str = "gitrun-setup-progress";
const FIRST_SETUP_TOTAL: u8 = 9;

fn emit_first_setup_event(
    app: &AppHandle,
    phase: u8,
    message: impl Into<String>,
    stream: &str,
    done: bool,
    success: bool,
) {
    if let Err(error) = app.emit(
        FIRST_SETUP_EVENT,
        FirstSetupEvent {
            phase: phase.min(FIRST_SETUP_TOTAL),
            total: FIRST_SETUP_TOTAL,
            message: message.into(),
            stream: stream.to_owned(),
            done,
            success,
        },
    ) {
        eprintln!("GitRun dashboard: unable to emit setup progress event: {error}");
    }
}

fn parse_setup_progress_line(line: &str) -> Option<(u8, String)> {
    let rest = line.strip_prefix("[GitRun setup] [")?;
    let (fraction, message) = rest.split_once("] ")?;
    let (step, total) = fraction.split_once('/')?;
    let step = step.parse::<u8>().ok()?;
    if total.parse::<u8>().ok()? != FIRST_SETUP_TOTAL {
        return None;
    }
    Some((step.min(FIRST_SETUP_TOTAL), message.to_owned()))
}

enum SetupChildOutput {
    Line { stream: &'static str, line: String },
    Done,
}

fn spawn_setup_reader<R>(reader: R, stream: &'static str, sender: mpsc::Sender<SetupChildOutput>)
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if sender
                        .send(SetupChildOutput::Line { stream, line })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(SetupChildOutput::Line {
                        stream,
                        line: format!("unable to read {stream}: {error}"),
                    });
                    break;
                }
            }
        }
        let _ = sender.send(SetupChildOutput::Done);
    });
}

fn stream_privileged_command(
    app: &AppHandle,
    pkexec: &std::path::Path,
    cli: &std::path::Path,
    args: &[String],
) -> Result<std::process::ExitStatus, String> {
    emit_first_setup_event(
        app,
        0,
        "Waiting for administrator authorization…",
        "system",
        false,
        false,
    );

    let mut command = std::process::Command::new(pkexec);
    command.arg(cli);
    if !args.iter().any(|argument| argument == "--no-cat") {
        command.arg("--no-cat");
    }
    command.args(args);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("unable to start privileged GitRun setup: {error}"))?;

    emit_first_setup_event(
        app,
        0,
        "Administrator authorization requested. Complete the system authentication dialog to continue.",
        "system",
        false,
        false,
    );

    let (sender, receiver) = mpsc::channel();
    let mut expected_readers = 0usize;

    if let Some(stdout) = child.stdout.take() {
        expected_readers += 1;
        spawn_setup_reader(stdout, "stdout", sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        expected_readers += 1;
        spawn_setup_reader(stderr, "stderr", sender.clone());
    }
    drop(sender);

    let mut finished_readers = 0usize;
    let mut current_phase = 0u8;
    let mut status = None;

    while finished_readers < expected_readers || status.is_none() {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(SetupChildOutput::Line { stream, line }) => {
                if let Some((phase, _status_message)) = parse_setup_progress_line(&line) {
                    current_phase = phase;
                }
                emit_first_setup_event(app, current_phase, line, stream, false, false);
            }
            Ok(SetupChildOutput::Done) => {
                finished_readers += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if status.is_none() {
                    status = Some(child.wait().map_err(|error| {
                        format!("unable to wait for privileged setup: {error}")
                    })?);
                }
                break;
            }
        }

        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| format!("unable to inspect privileged setup: {error}"))?;
        }
    }

    let status = match status {
        Some(status) => status,
        None => child
            .wait()
            .map_err(|error| format!("unable to wait for privileged setup: {error}"))?,
    };

    Ok(status)
}

#[tauri::command]
async fn uninstall_gitrun(app: AppHandle) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("graphical uninstall currently targets Linux x86_64".into());
    }

    let pkexec = pkexec_path().ok_or(
        "trusted pkexec was not found at a root-owned, non-writable system path; graphical uninstall cannot safely continue",
    )?;
    let cli = dashboard_cli_path(&app)
        .ok_or("GitRun CLI executable was not found; cannot run the privileged uninstall")?;

    let app_for_uninstall = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = stream_privileged_command(
            &app_for_uninstall,
            &pkexec,
            &cli,
            &["--no-cat".to_owned(), "--uninstall-root".to_owned()],
        );
        match result {
            Ok(status) if status.success() => {
                emit_first_setup_event(
                    &app_for_uninstall,
                    0,
                    "GitRun uninstall completed successfully.",
                    "system",
                    true,
                    true,
                );
                Ok(())
            }
            Ok(status) => {
                emit_first_setup_event(
                    &app_for_uninstall,
                    0,
                    format!("Privileged uninstall failed with status {status}."),
                    "system",
                    true,
                    false,
                );
                Err(format!(
                    "privileged GitRun uninstall failed with status {status}"
                ))
            }
            Err(error) => {
                emit_first_setup_event(&app_for_uninstall, 0, error.clone(), "system", true, false);
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| format!("privileged GitRun uninstall task failed: {error}"))??;

    Ok(())
}

fn stream_unprivileged_command(
    app: &AppHandle,
    cli: &std::path::Path,
    args: &[String],
) -> Result<std::process::ExitStatus, String> {
    emit_first_setup_event(
        app,
        0,
        "Starting the verified GitRun reinstall helper…",
        "system",
        false,
        false,
    );

    let mut command = std::process::Command::new(cli);
    if !args.iter().any(|argument| argument == "--no-cat") {
        command.arg("--no-cat");
    }
    let mut child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("unable to start GitRun reinstall helper: {error}"))?;

    let (sender, receiver) = mpsc::channel();
    let mut expected_readers = 0usize;
    if let Some(stdout) = child.stdout.take() {
        expected_readers += 1;
        spawn_setup_reader(stdout, "stdout", sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        expected_readers += 1;
        spawn_setup_reader(stderr, "stderr", sender.clone());
    }
    drop(sender);

    let mut finished_readers = 0usize;
    let mut current_phase = 0u8;
    let mut status = None;
    while finished_readers < expected_readers || status.is_none() {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(SetupChildOutput::Line { stream, line }) => {
                if let Some((phase, _)) = parse_setup_progress_line(&line) {
                    current_phase = phase;
                }
                emit_first_setup_event(app, current_phase, line, stream, false, false);
            }
            Ok(SetupChildOutput::Done) => finished_readers += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if status.is_none() {
                    status = Some(child.wait().map_err(|error| {
                        format!("unable to wait for GitRun reinstall helper: {error}")
                    })?);
                }
                break;
            }
        }
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| format!("unable to inspect GitRun reinstall helper: {error}"))?;
        }
    }

    match status {
        Some(status) => Ok(status),
        None => child
            .wait()
            .map_err(|error| format!("unable to wait for GitRun reinstall helper: {error}")),
    }
}

#[tauri::command]
async fn reinstall_gitrun(app: AppHandle) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("graphical reinstall currently targets Linux x86_64".into());
    }
    let cli = dashboard_cli_path(&app)
        .ok_or("trusted GitRun CLI was not found; the reinstall cannot safely continue")?;

    let app_for_reinstall = app.clone();
    let cli_for_reinstall = cli.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = stream_unprivileged_command(
            &app_for_reinstall,
            &cli_for_reinstall,
            &["--no-cat".to_owned(), "reinstall".to_owned()],
        );
        match result {
            Ok(status) if status.success() => {
                emit_first_setup_event(
                    &app_for_reinstall,
                    0,
                    "Verified latest GitRun release installed. Restarting the dashboard…",
                    "system",
                    true,
                    true,
                );
                std::process::Command::new(&cli_for_reinstall)
                    .arg("--no-cat")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|error| {
                        format!("GitRun reinstalled but the dashboard could not restart: {error}")
                    })?;
                Ok(())
            }
            Ok(status) => {
                emit_first_setup_event(
                    &app_for_reinstall,
                    0,
                    format!("GitRun reinstall failed with status {status}."),
                    "system",
                    true,
                    false,
                );
                Err(format!("GitRun reinstall failed with status {status}"))
            }
            Err(error) => {
                emit_first_setup_event(&app_for_reinstall, 0, error.clone(), "system", true, false);
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| format!("GitRun reinstall task failed: {error}"))??;

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirstSetupRequest {
    auth_mode: String,
    token: String,
    repositories: String,
    app_id: String,
    installation_id: String,
    private_key_path: String,
    runner_profile: String,
    reinstall: bool,
}

#[tauri::command]
async fn run_first_setup(app: AppHandle, request: FirstSetupRequest) -> Result<(), String> {
    let FirstSetupRequest {
        auth_mode,
        token,
        repositories: repository_input,
        app_id,
        installation_id,
        private_key_path,
        runner_profile,
        reinstall,
    } = request;

    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("graphical first-run setup currently targets Linux x86_64".into());
    }
    if reinstall {
        if configured_config_path().is_none() {
            return Err(
                "GitRun is not currently configured; use the normal first-run setup instead".into(),
            );
        }
    } else if configured_config_path().is_some() {
        return Err("GitRun is already configured; graphical first-run setup is only available before initial setup".into());
    }

    if !matches!(runner_profile.as_str(), "minimum" | "workbench") {
        return Err("runner profile must be minimum or workbench".into());
    }

    let auth = build_first_setup_auth(
        &auth_mode,
        &token,
        &app_id,
        &installation_id,
        &private_key_path,
    )?;

    let repositories = repository_input
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .collect::<Vec<_>>();

    if repositories.is_empty()
        || repositories
            .iter()
            .any(|repo| !valid_setup_repository(repo))
    {
        return Err("Enter at least one repository as owner/repository".into());
    }

    let pkexec = pkexec_path().ok_or(
        "trusted pkexec was not found at a root-owned, non-writable system path; graphical setup cannot safely continue",
    )?;

    let cli = dashboard_cli_path(&app).ok_or(
        "GitRun CLI executable was not found; install gitrun alongside the dashboard before running graphical setup",
    )?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gitrun-setup-tauri-{}-{unique}.conf",
        std::process::id()
    ));

    let auth_lines = match &auth {
        BootstrapAuth::Pat(token) => format!("AUTH_MODE=pat\nGITHUB_TOKEN={}\n", token),
        BootstrapAuth::GitHubApp {
            app_id,
            installation_id,
            private_key_path,
        } => format!(
            "AUTH_MODE=app\nGITRUN_GITHUB_APP_ID={}\nGITRUN_GITHUB_APP_INSTALLATION_ID={}\nGITRUN_GITHUB_APP_PRIVATE_KEY_PATH={}\n",
            app_id, installation_id, private_key_path
        ),
    };
    let payload = format!(
        "{auth_lines}GITRUN_REPOSITORIES={}\nRUNNER_PROFILE={}\n",
        repositories.join(","),
        runner_profile
    );
    write_setup_request(&path, &payload)?;

    let app_for_setup = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let operation = if reinstall {
            "--reinstall-root"
        } else {
            "--install-root"
        };
        let result = stream_privileged_command(
            &app_for_setup,
            &pkexec,
            &cli,
            &[operation.to_owned(), path.to_string_lossy().into_owned()],
        );

        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                let warning = format!(
                    "WARNING: setup finished running, but the temporary request file containing credentials could not be removed: {error}"
                );
                eprintln!("GitRun dashboard: {warning}");
                emit_first_setup_event(
                    &app_for_setup,
                    0,
                    warning,
                    "stderr",
                    false,
                    false,
                );
            }
        }

        match result {
            Ok(status) if status.success() => {
                emit_first_setup_event(
                    &app_for_setup,
                    FIRST_SETUP_TOTAL,
                    "GitRun setup completed successfully.",
                    "system",
                    true,
                    true,
                );
                Ok(())
            }
            Ok(status) => {
                emit_first_setup_event(
                    &app_for_setup,
                    0,
                    format!("Privileged setup failed with status {status}."),
                    "system",
                    true,
                    false,
                );
                Err(format!(
                    "privileged GitRun setup failed with status {status}"
                ))
            }
            Err(error) => {
                emit_first_setup_event(&app_for_setup, 0, error.clone(), "system", true, false);
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| format!("privileged GitRun setup task failed: {error}"))??;

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_privileged_cli_helper(command_name: &str, arguments: &[String]) -> Result<String, String> {
    if control_manager_enabled() && !current_process_is_root() {
        let value = match command_name {
            "--read-gsr-events-root" if arguments.len() == 1 => {
                let limit = arguments[0]
                    .parse::<usize>()
                    .map_err(|_| "invalid GSR event limit".to_owned())?;
                if !(1..=500).contains(&limit) {
                    return Err("GSR event limit must be between 1 and 500".into());
                }
                control_manager_request("read", "gsr_events", serde_json::json!({ "limit": limit }))?
            }
            "--read-logic-rules-root" if arguments.is_empty() => {
                control_manager_request("read", "logic_rules", serde_json::json!({}))?
            }
            "--read-gitrun-settings-root" if arguments.is_empty() => {
                control_manager_request("read", "gitrun_settings", serde_json::json!({}))?
            }
            "--read-vm-configs-root" if arguments.is_empty() => {
                control_manager_request("read", "vm_configs", serde_json::json!({}))?
            }
            "--read-pending-hypervisor-decisions-root" if arguments.is_empty() => {
                control_manager_request("read", "hypervisor_decisions", serde_json::json!({}))?
            }
            "--respond-hypervisor-decision-root"
                if arguments.len() == 3 && arguments[0] == "--" =>
            {
                control_manager_request(
                    "write",
                    "hypervisor_respond",
                    serde_json::json!({
                        "vm_name": arguments[1],
                        "choice": arguments[2],
                        "confirmed": true
                    }),
                )?
            }
            _ => return Err(format!(
                "protected helper operation is not available through the Control Manager: {command_name}"
            )),
        };
        return serde_json::to_string(&value)
            .map_err(|error| format!("encode Control Manager response: {error}"));
    }

    let cli = trusted_system_cli_path().ok_or(
        "A trusted system-installed GitRun CLI was not found; protected GitRun state cannot be accessed safely.",
    )?;
    let output = authorized_cli_command(&cli)?
        .arg(command_name)
        .args(arguments)
        .output()
        .map_err(|error| format!("unable to launch protected GitRun state helper: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!(
                "authorized GitRun state helper exited with status {}",
                output.status
            )
        } else {
            format!("authorized GitRun state helper failed: {detail}")
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("GitRun state helper returned invalid UTF-8: {error}"))
}

fn list_gsr_events_for_dashboard(
    config: &Config,
    limit: usize,
) -> Result<Vec<gitrun_gsr::SecurityEvent>, String> {
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        let output = run_privileged_cli_helper(
            "--read-gsr-events-root",
            &[limit.clamp(1, 500).to_string()],
        )?;
        return serde_json::from_str(output.trim())
            .map_err(|error| format!("decode authorized GSR event response: {error}"));
    }

    let path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    gitrun_gsr::events::read_last(&path, limit.clamp(1, 500)).map_err(|error| error.to_string())
}

fn count_critical_events_for_dashboard(config: &Config, cutoff: u64) -> Result<u32, String> {
    // Only unprivileged developer installs call this helper. System installs
    // intentionally leave this aggregate unavailable on Overview and prompt
    // for authorization only when the operator opens the GSR event view.
    let path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    gitrun_gsr::events::count_critical_since(&path, cutoff).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct OverviewData {
    pub repositories: Vec<String>,
    pub min_runners: u32,
    pub max_runners: u32,
    pub vault_enabled: bool,
    pub gsr_watching: bool,
    pub recent_critical_events: Option<u32>,
    pub warnings: Vec<String>,
}

#[tauri::command]
fn get_overview() -> Result<OverviewData, String> {
    let config = load_config()?;
    let cutoff = recent_event_cutoff();
    let mut warnings = Vec::new();
    #[cfg(target_os = "linux")]
    let system_install =
        trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file();
    #[cfg(not(target_os = "linux"))]
    let system_install = false;
    let recent_critical_events = if system_install {
        // The queue is root-only and includes security-sensitive details.
        // Avoid prompting for elevation during dashboard startup; the GSR page
        // retrieves it through its explicit, fixed-purpose helper when opened.
        None
    } else {
        match count_critical_events_for_dashboard(&config, cutoff) {
            Ok(count) => Some(count),
            Err(error) => {
                warnings.push(format!("Critical-event queue could not be read: {error}"));
                None
            }
        }
    };
    let gsr_watching = process_name_running("gitrun-gsr");

    Ok(OverviewData {
        repositories: config.repositories.clone(),
        min_runners: config.min_runners,
        max_runners: config.max_runners,
        vault_enabled: !config.vault_dir.trim().is_empty(),
        gsr_watching,
        recent_critical_events,
        warnings,
    })
}

// ---------------------------------------------------------------------
// Per-repo detail (includes Logic Containers rules for that repo, per the
// operator's direction that Logic Containers lives under the repo tab, not
// as a standalone top-level section)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct DashboardRunnerRecord {
    repository: String,
    id: u64,
    name: String,
    status: String,
    online: bool,
    busy: bool,
    kind: String,
    container_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardWorkflowRunSummary {
    repository: String,
    total: u32,
    queued: u32,
    in_progress: u32,
    succeeded: u32,
    failed: u32,
    cancelled: u32,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardActiveWorkflowRun {
    repository: String,
    id: u64,
    name: String,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardRunnerSnapshot {
    checked_at: u64,
    stale: bool,
    complete: bool,
    repositories_checked: usize,
    repositories_total: usize,
    workflow_repositories_checked: usize,
    workflow_runs: Vec<DashboardWorkflowRunSummary>,
    active_workflows: Vec<DashboardActiveWorkflowRun>,
    runners: Vec<DashboardRunnerRecord>,
    warnings: Vec<String>,
}

#[derive(Clone)]
struct CachedDashboardRunnerSnapshot {
    fetched_at: Instant,
    config_key: u64,
    value: DashboardRunnerSnapshot,
}

fn dashboard_config_cache_key() -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut variables = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("GITRUN_"))
        .collect::<Vec<_>>();
    variables.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    variables.hash(&mut hasher);
    std::env::var_os("GITHUB_TOKEN")
        .unwrap_or_default()
        .hash(&mut hasher);

    let config_path = configured_config_path();
    if let Some(path) = config_path.as_ref() {
        path.hash(&mut hasher);
        if let Ok(contents) = std::fs::read(path) {
            contents.hash(&mut hasher);
        }
    }
    let private_key_path = std::env::var_os("GITRUN_GITHUB_APP_PRIVATE_KEY_PATH")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            config_path
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|contents| {
                    contents.lines().find_map(|line| {
                        let (key, value) = line.trim().split_once('=')?;
                        (key.trim() == "GITRUN_GITHUB_APP_PRIVATE_KEY_PATH").then(|| {
                            std::path::PathBuf::from(
                                value.trim().trim_matches('"').trim_matches('\''),
                            )
                        })
                    })
                })
                .filter(|path| !path.as_os_str().is_empty())
        });
    if let Some(key_path) = private_key_path {
        if let Ok(contents) = std::fs::read(key_path) {
            contents.hash(&mut hasher);
        }
    }

    hasher.finish()
}

static DASHBOARD_RUNNER_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<CachedDashboardRunnerSnapshot>>,
> = std::sync::OnceLock::new();

fn dashboard_github_client(config: &Config) -> Result<gitrun_scheduler::GitHubClient, String> {
    let auth = match configured_config_path() {
        Some(path) => gitrun_core::GitHubAuth::from_config_file(config, &path),
        None => gitrun_core::GitHubAuth::from_config(config),
    }
    .map_err(|error| error.to_string())?;
    let connect_timeout = Duration::from_secs(config.github_connect_timeout);
    let request_timeout = Duration::from_secs(config.github_request_timeout);
    match auth {
        gitrun_core::GitHubAuth::Pat(token) => {
            gitrun_scheduler::GitHubClient::with_timeouts(token, connect_timeout, request_timeout)
                .map_err(|error| error.to_string())
        }
        gitrun_core::GitHubAuth::App(auth) => {
            gitrun_scheduler::GitHubClient::with_app_auth(auth, connect_timeout, request_timeout)
                .map_err(|error| error.to_string())
        }
    }
}

/// Force-cancel a configured repository's workflow run using GitHub's run-level API.
/// This intentionally affects every job in that workflow run, not one individual job.
#[tauri::command]
fn fast_stop_workflow_run(repository: String, run_id: u64) -> Result<String, String> {
    if run_id == 0 {
        return Err("workflow run ID must be greater than zero".into());
    }
    let config = load_config()?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == &repository)
    {
        return Err(format!(
            "refusing to cancel run for unconfigured repository {repository:?}"
        ));
    }
    let client = dashboard_github_client(&config)?;
    client
        .force_cancel_workflow_run(&repository, run_id)
        .map_err(|error| {
            format!("GitHub force-cancel failed for {repository} run {run_id}: {error}")
        })?;
    if let Some(cache) = DASHBOARD_RUNNER_CACHE.get() {
        let mut cache = cache
            .lock()
            .map_err(|_| "runner snapshot cache lock poisoned".to_owned())?;
        *cache = None;
    }
    Ok(format!("GitHub accepted force-cancellation for workflow run {run_id} in {repository}; all jobs in the run are affected."))
}

#[tauri::command]
fn get_runner_snapshot(force_refresh: Option<bool>) -> Result<DashboardRunnerSnapshot, String> {
    const CACHE_TTL: Duration = Duration::from_secs(60);
    let cache = DASHBOARD_RUNNER_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let config_key = dashboard_config_cache_key();
    let cached = cache
        .lock()
        .map_err(|_| "runner snapshot cache lock poisoned".to_owned())?
        .clone()
        .filter(|snapshot| snapshot.config_key == config_key);
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached.as_ref().filter(|snapshot| {
            let ttl = if snapshot.value.stale {
                Duration::from_secs(5)
            } else {
                CACHE_TTL
            };
            snapshot.fetched_at.elapsed() < ttl
        }) {
            return Ok(snapshot.value.clone());
        }
    }

    let fresh = (|| -> Result<DashboardRunnerSnapshot, String> {
        let config = load_config()?;
        let client = dashboard_github_client(&config)?;
        let mut runners = Vec::new();
        let mut workflow_runs = Vec::new();
        let mut active_workflows = Vec::new();
        let mut warnings = Vec::new();
        let mut repositories_checked = 0usize;
        let mut workflow_repositories_checked = 0usize;
        let mut active_workflow_repositories_checked = 0usize;
        for repo in &config.repositories {
            // These GitHub endpoints are independent. Fetch them concurrently so a
            // slow workflow-history or active-run query does not serialize runner state.
            let (runner_result, workflow_result, active_result) = std::thread::scope(|scope| {
                let runner_query =
                    scope.spawn(|| client.list_runners(repo).map_err(|error| error.to_string()));
                let workflow_query = scope.spawn(|| {
                    client
                        .recent_workflow_run_summary(repo, 100)
                        .map_err(|error| error.to_string())
                });
                let active_query = scope.spawn(|| {
                    client
                        .list_active_workflow_runs(repo)
                        .map_err(|error| error.to_string())
                });
                let runners = runner_query
                    .join()
                    .unwrap_or_else(|_| Err("runner query worker panicked".to_owned()));
                let workflows = workflow_query
                    .join()
                    .unwrap_or_else(|_| Err("workflow summary worker panicked".to_owned()));
                let active = active_query
                    .join()
                    .unwrap_or_else(|_| Err("active workflow query worker panicked".to_owned()));
                (runners, workflows, active)
            });

            let remote_runners = match runner_result {
                Ok(value) => {
                    repositories_checked += 1;
                    Some(value)
                }
                Err(error) => {
                    let message = if error.contains("403")
                        || error
                            .to_ascii_lowercase()
                            .contains("resource not accessible by integration")
                    {
                        format!(
                            "{repo}: GitHub runner inventory is unavailable (403). The GitHub App needs repository Administration: read permission and its installation permissions must be refreshed; a classic PAT needs repo scope and repository admin access. Runner count remains unknown until GitHub grants this access. Original error: {error}"
                        )
                    } else {
                        format!("{repo}: GitHub runner query failed: {error}")
                    };
                    warnings.push(message);
                    None
                }
            };
            match workflow_result {
                Ok(summary) => {
                    workflow_repositories_checked += 1;
                    workflow_runs.push(DashboardWorkflowRunSummary {
                        repository: repo.clone(),
                        total: summary.total,
                        queued: summary.queued,
                        in_progress: summary.in_progress,
                        succeeded: summary.succeeded,
                        failed: summary.failed,
                        cancelled: summary.cancelled,
                    });
                }
                Err(error) => {
                    warnings.push(format!("{repo}: workflow-run summary failed: {error}"))
                }
            }
            match active_result {
                Ok(active) => {
                    active_workflow_repositories_checked += 1;
                    active_workflows.extend(active.into_iter().map(|run| {
                        DashboardActiveWorkflowRun {
                            repository: repo.clone(),
                            id: run.id,
                            name: run.name,
                            status: "in_progress".into(),
                        }
                    }));
                }
                Err(error) => {
                    warnings.push(format!("{repo}: active workflow inventory failed: {error}"))
                }
            }
            let Some(remote_runners) = remote_runners else {
                continue;
            };
            let containers = match gitrun_scheduler::docker::managed_containers(repo) {
                Ok(value) => Some(value),
                Err(error) => {
                    warnings.push(format!(
                        "{repo}: Docker container state unavailable: {error}"
                    ));
                    None
                }
            };
            for runner in remote_runners {
                let container = containers
                    .as_ref()
                    .and_then(|items| items.iter().find(|item| item.name == runner.name));
                let kind = match (container, containers.is_some()) {
                    (Some(item), _) if item.permanent => "Permanent",
                    (Some(_), _) => "Dynamic",
                    (None, true) => "GitHub-only",
                    (None, false) => "Unknown",
                };
                runners.push(DashboardRunnerRecord {
                    repository: repo.clone(),
                    id: runner.id,
                    name: runner.name,
                    status: runner.status.clone(),
                    online: runner.status.eq_ignore_ascii_case("online"),
                    busy: runner.busy,
                    kind: kind.into(),
                    container_status: container.map(|item| item.status.clone()),
                });
            }
        }
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Ok(DashboardRunnerSnapshot {
            checked_at,
            stale: false,
            complete: repositories_checked == config.repositories.len()
                && workflow_repositories_checked == config.repositories.len()
                && active_workflow_repositories_checked == config.repositories.len()
                && warnings.is_empty(),
            repositories_checked,
            repositories_total: config.repositories.len(),
            workflow_repositories_checked,
            workflow_runs,
            active_workflows,
            runners,
            warnings,
        })
    })();

    match fresh {
        Ok(snapshot) => {
            *cache
                .lock()
                .map_err(|_| "runner snapshot cache lock poisoned".to_owned())? =
                Some(CachedDashboardRunnerSnapshot {
                    fetched_at: Instant::now(),
                    config_key,
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
        Err(error) => {
            let mut snapshot =
                cached
                    .map(|cached| cached.value)
                    .unwrap_or_else(|| DashboardRunnerSnapshot {
                        checked_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|duration| duration.as_secs())
                            .unwrap_or(0),
                        stale: true,
                        complete: false,
                        repositories_checked: 0,
                        repositories_total: load_config()
                            .map(|config| config.repositories.len())
                            .unwrap_or(0),
                        workflow_repositories_checked: 0,
                        workflow_runs: Vec::new(),
                        active_workflows: Vec::new(),
                        runners: Vec::new(),
                        warnings: Vec::new(),
                    });
            snapshot.stale = true;
            snapshot
                .warnings
                .retain(|warning| !warning.starts_with("Runner refresh unavailable:"));
            snapshot
                .warnings
                .push(format!("Runner refresh unavailable: {error}"));
            *cache
                .lock()
                .map_err(|_| "runner snapshot cache lock poisoned".to_owned())? =
                Some(CachedDashboardRunnerSnapshot {
                    fetched_at: Instant::now(),
                    config_key,
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DashboardHostResourceSnapshot {
    sampled_at: u64,
    stale: bool,
    cpu_percent: f32,
    memory_percent: f32,
    disk_percent: f32,
    error: Option<String>,
}

#[derive(Clone)]
struct CachedDashboardHostResources {
    sampled_at: Instant,
    value: DashboardHostResourceSnapshot,
}

static DASHBOARD_HOST_RESOURCE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<CachedDashboardHostResources>>,
> = std::sync::OnceLock::new();

#[tauri::command]
fn get_host_resource_snapshot(
    force_refresh: Option<bool>,
) -> Result<DashboardHostResourceSnapshot, String> {
    const CACHE_TTL: Duration = Duration::from_secs(15);
    let cache = DASHBOARD_HOST_RESOURCE_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let cached = cache
        .lock()
        .map_err(|_| "host resource cache lock poisoned".to_owned())?
        .clone();
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached
            .as_ref()
            .filter(|snapshot| snapshot.sampled_at.elapsed() < CACHE_TTL)
        {
            return Ok(snapshot.value.clone());
        }
    }
    let sample = load_config().and_then(|config| {
        gitrun_scheduler::resource_pressure::sample(&config).map_err(|error| error.to_string())
    });
    match sample {
        Ok(sample) => {
            let sampled_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let snapshot = DashboardHostResourceSnapshot {
                sampled_at,
                stale: false,
                cpu_percent: sample.cpu_percent,
                memory_percent: sample.memory_percent,
                disk_percent: sample.disk_percent,
                error: None,
            };
            *cache
                .lock()
                .map_err(|_| "host resource cache lock poisoned".to_owned())? =
                Some(CachedDashboardHostResources {
                    sampled_at: Instant::now(),
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
        Err(error) => {
            if let Some(mut snapshot) = cached.map(|cached| cached.value) {
                snapshot.stale = true;
                snapshot.error = Some(format!(
                    "Resource probe failed; showing cached values: {error}"
                ));
                *cache
                    .lock()
                    .map_err(|_| "host resource cache lock poisoned".to_owned())? =
                    Some(CachedDashboardHostResources {
                        sampled_at: Instant::now(),
                        value: snapshot.clone(),
                    });
                Ok(snapshot)
            } else {
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DashboardRepositoryActivity {
    repository: String,
    stars: u64,
    forks: u64,
    open_pull_requests: u64,
    merged_pull_requests: u64,
    pushed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardRepositoryActivitySnapshot {
    checked_at: u64,
    stale: bool,
    complete: bool,
    repositories_checked: usize,
    repositories_total: usize,
    repositories: Vec<DashboardRepositoryActivity>,
    warnings: Vec<String>,
}

#[derive(Clone)]
struct CachedDashboardRepositoryActivity {
    fetched_at: Instant,
    config_key: u64,
    value: DashboardRepositoryActivitySnapshot,
}

static DASHBOARD_REPOSITORY_ACTIVITY_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<CachedDashboardRepositoryActivity>>,
> = std::sync::OnceLock::new();

#[tauri::command]
fn get_repository_activity(
    force_refresh: Option<bool>,
) -> Result<DashboardRepositoryActivitySnapshot, String> {
    const CACHE_TTL: Duration = Duration::from_secs(300);
    let cache = DASHBOARD_REPOSITORY_ACTIVITY_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let config_key = dashboard_config_cache_key();
    let cached = cache
        .lock()
        .map_err(|_| "repository activity cache lock poisoned".to_owned())?
        .clone()
        .filter(|snapshot| snapshot.config_key == config_key);
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached.as_ref().filter(|snapshot| {
            let ttl = if snapshot.value.stale {
                Duration::from_secs(5)
            } else {
                CACHE_TTL
            };
            snapshot.fetched_at.elapsed() < ttl
        }) {
            return Ok(snapshot.value.clone());
        }
    }

    let fresh = (|| -> Result<DashboardRepositoryActivitySnapshot, String> {
        let config = load_config()?;
        let client = dashboard_github_client(&config)?;
        let mut repositories = Vec::new();
        let mut warnings = Vec::new();
        let mut repositories_checked = 0usize;
        for repo in &config.repositories {
            match client.repository_activity_summary(repo) {
                Ok(item) => {
                    repositories_checked += 1;
                    repositories.push(DashboardRepositoryActivity {
                        repository: item.repository,
                        stars: item.stars,
                        forks: item.forks,
                        open_pull_requests: item.open_pull_requests,
                        merged_pull_requests: item.merged_pull_requests,
                        pushed_at: item.pushed_at,
                    });
                }
                Err(error) => {
                    warnings.push(format!("{repo}: repository activity query failed: {error}"))
                }
            }
        }
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Ok(DashboardRepositoryActivitySnapshot {
            checked_at,
            stale: false,
            complete: repositories_checked == config.repositories.len() && warnings.is_empty(),
            repositories_checked,
            repositories_total: config.repositories.len(),
            repositories,
            warnings,
        })
    })();

    match fresh {
        Ok(snapshot) => {
            *cache
                .lock()
                .map_err(|_| "repository activity cache lock poisoned".to_owned())? =
                Some(CachedDashboardRepositoryActivity {
                    fetched_at: Instant::now(),
                    config_key,
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
        Err(error) => {
            let mut snapshot = cached.map(|cached| cached.value).unwrap_or_else(|| {
                DashboardRepositoryActivitySnapshot {
                    checked_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0),
                    stale: true,
                    complete: false,
                    repositories_checked: 0,
                    repositories_total: load_config()
                        .map(|config| config.repositories.len())
                        .unwrap_or(0),
                    repositories: Vec::new(),
                    warnings: Vec::new(),
                }
            });
            snapshot.stale = true;
            snapshot
                .warnings
                .retain(|warning| !warning.starts_with("Repository activity refresh unavailable:"));
            snapshot
                .warnings
                .push(format!("Repository activity refresh unavailable: {error}"));
            *cache
                .lock()
                .map_err(|_| "repository activity cache lock poisoned".to_owned())? =
                Some(CachedDashboardRepositoryActivity {
                    fetched_at: Instant::now(),
                    config_key,
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
    }
}

fn logic_rules_for_dashboard(
    config: &Config,
) -> Result<Vec<gitrun_scheduler::logic_containers::LogicRule>, String> {
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        let raw = run_privileged_cli_helper("--read-logic-rules-root", &[])?;
        return serde_json::from_str(raw.trim())
            .map_err(|error| format!("decode authorized Logic Containers rules: {error}"));
    }

    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    gitrun_scheduler::logic_containers::load_rules(&rules_path).map_err(|error| error.to_string())
}

#[derive(Debug, Serialize)]
pub struct RepoDetail {
    pub repo: String,
    pub vault_groups: Vec<String>,
    pub logic_rules: Vec<gitrun_scheduler::logic_containers::LogicRule>,
    pub docker_policy: gitrun_core::DockerPolicy,
}

#[tauri::command]
fn get_repo_detail(repo: String) -> Result<RepoDetail, String> {
    let config = load_config()?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == &repo)
    {
        return Err(format!("repository is not configured in GitRun: {repo}"));
    }
    let all_rules = logic_rules_for_dashboard(&config)?;
    let settings = gitrun_settings_for_dashboard(&config)?;
    let docker_policy = settings.effective_for_repository(&repo).docker;
    Ok(RepoDetail {
        vault_groups: config.vault_groups_for_repo(&repo),
        repo,
        logic_rules: all_rules,
        docker_policy,
    })
}

// ---------------------------------------------------------------------
// GitVault
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct VaultSecretSummary {
    pub name: String,
    pub scope: ScopeDto,
    pub updated_at: u64,
}

/// JSON-friendly mirror of `gitrun_vault::Scope`. Kept as an explicit DTO
/// rather than deriving Serialize directly on the library's Scope, so the
/// wire format is a deliberate choice, not whatever serde's default enum
/// representation happens to produce.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ScopeDto {
    Global,
    Group(String),
    Repo(String),
}

impl From<Scope> for ScopeDto {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Global => ScopeDto::Global,
            Scope::Group(g) => ScopeDto::Group(g),
            Scope::Repo(r) => ScopeDto::Repo(r),
        }
    }
}

impl From<ScopeDto> for Scope {
    fn from(dto: ScopeDto) -> Self {
        match dto {
            ScopeDto::Global => Scope::Global,
            ScopeDto::Group(g) => Scope::Group(g),
            ScopeDto::Repo(r) => Scope::Repo(r),
        }
    }
}

fn open_vault(config: &Config) -> Result<Vault, String> {
    if config.vault_dir.trim().is_empty() {
        return Err("GitVault is not configured (GITRUN_VAULT_DIR is empty)".into());
    }
    let sink = VaultToGsrBridge::new(&config.state_dir);
    Vault::open_with_sink(&config.vault_dir, Box::new(sink)).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_vault_secrets() -> Result<Vec<VaultSecretSummary>, String> {
    let config = load_config()?;
    let vault = open_vault(&config)?;
    Ok(vault
        .list()
        .into_iter()
        .map(|(name, scope, updated_at)| VaultSecretSummary {
            name,
            scope: scope.into(),
            updated_at,
        })
        .collect())
}

/// Sets a secret's *value*. Deliberately one-directional: there is no
/// `get_vault_secret` command exposed to the frontend — decrypted secret
/// values are never sent to the webview at all. This isn't an oversight;
/// showing plaintext secrets in a UI (even briefly, even locally) is a
/// bigger exposure surface than the dashboard needs to have. An operator
/// who needs to verify a value should re-enter it (overwriting is always
/// available) rather than the dashboard round-tripping it back to them.
#[tauri::command]
fn set_vault_secret(name: String, value: String, scope: ScopeDto) -> Result<(), String> {
    const MAX_VAULT_SECRET_BYTES: usize = 1024 * 1024;
    if value.len() > MAX_VAULT_SECRET_BYTES {
        return Err("GitVault secret exceeds the 1 MiB safety limit".into());
    }
    let config = load_config()?;
    let mut vault = open_vault(&config)?;
    vault
        .set_scoped(&name, &value, scope.into())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_vault_secret(name: String, scope: ScopeDto) -> Result<(), String> {
    let config = load_config()?;
    let mut vault = open_vault(&config)?;
    vault
        .delete_scoped(&name, &scope.into())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// GSR
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GsrStatus {
    pub watching: bool,
    pub watched_pid: Option<u32>,
}

#[tauri::command]
fn get_gsr_status() -> Result<GsrStatus, String> {
    let config = load_config()?;
    #[cfg(target_os = "linux")]
    let watched_pid =
        if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
            None
        } else {
            let pid_file = PathBuf::from(&config.state_dir).join("gitrun-autoscaler.pid");
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|pid| process_has_name(*pid, "gitrun-autoscaler"))
        };
    #[cfg(not(target_os = "linux"))]
    let watched_pid = {
        let pid_file = PathBuf::from(&config.state_dir).join("gitrun-autoscaler.pid");
        std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|pid| process_has_name(*pid, "gitrun-autoscaler"))
    };

    Ok(GsrStatus {
        watching: process_name_running("gitrun-gsr"),
        watched_pid,
    })
}

#[tauri::command]
fn list_gsr_events(limit: Option<usize>) -> Result<Vec<gitrun_gsr::SecurityEvent>, String> {
    let config = load_config()?;
    let limit = limit.unwrap_or(500).clamp(1, 500);
    let mut events = list_gsr_events_for_dashboard(&config, limit)?;
    events.reverse(); // most recent first
    Ok(events)
}

fn journal_units(source: &str) -> Result<Vec<&'static str>, String> {
    match source {
        "gsr" => Ok(vec!["gitrun-gsr.service"]),
        "scheduler" => Ok(vec!["gitrun.service"]),
        "all" => Ok(vec!["gitrun-gsr.service", "gitrun.service"]),
        _ => Err("unsupported log source; expected gsr, scheduler, or all".into()),
    }
}

#[tauri::command]
fn list_service_logs(source: String, limit: Option<usize>) -> Result<Vec<String>, String> {
    if control_manager_enabled() && !current_process_is_root() {
        let value = control_manager_request(
            "read",
            "service_logs",
            serde_json::json!({ "source": source, "limit": limit }),
        )?;
        return serde_json::from_value(value)
            .map_err(|error| format!("decode service logs from Control Manager: {error}"));
    }
    if !cfg!(target_os = "linux") {
        return Err("System journal logs are currently available only on Linux.".into());
    }
    let units = journal_units(&source)?;
    let limit = limit.unwrap_or(200).clamp(1, 500);
    let mut command = std::process::Command::new("/usr/bin/journalctl");
    command
        .arg("--no-pager")
        .arg("--output=short-iso-precise")
        .arg("--lines")
        .arg(limit.to_string());
    for unit in units {
        command.arg("--unit").arg(unit);
    }
    let output = command.output().map_err(|error| {
        format!("Unable to execute journalctl; GitRun service logs are unavailable: {error}")
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("journalctl exited with status {}", output.status)
        } else {
            format!("Unable to read GitRun service journal: {detail}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod journal_log_tests {
    use super::journal_units;

    #[test]
    fn journal_units_are_allowlisted() {
        assert_eq!(journal_units("gsr").unwrap(), vec!["gitrun-gsr.service"]);
        assert_eq!(journal_units("scheduler").unwrap(), vec!["gitrun.service"]);
        assert_eq!(
            journal_units("all").unwrap(),
            vec!["gitrun-gsr.service", "gitrun.service"]
        );
        assert!(journal_units("arbitrary.service").is_err());
    }
}

// ---------------------------------------------------------------------
// zizmor (optional third-party workflow analyzer) — see
// `gitrun_core::workflow_validation` for the full design rationale. The
// dashboard's "Learn more" page (rendered entirely from `get_zizmor_info`,
// so the credit/license text lives in exactly one place in the codebase)
// and its consent dialog both go through these two commands.
// ---------------------------------------------------------------------

/// Static credit/license info for the "Learn more" page, plus whether the
/// operator has already accepted the terms and whether the binary is
/// already present, so the dashboard can render the right button state
/// (Enable / already enabled / Learn more) without a second round trip.
#[derive(Debug, Serialize)]
pub struct ZizmorDashboardInfo {
    #[serde(flatten)]
    pub info: gitrun_core::ZizmorInfo,
    pub license_accepted: bool,
    pub enabled: bool,
    pub already_installed: bool,
}

#[tauri::command]
fn get_zizmor_info() -> Result<ZizmorDashboardInfo, String> {
    let config = load_config()?;
    let already_installed = gitrun_core::zizmor_is_installed();
    Ok(ZizmorDashboardInfo {
        info: gitrun_core::zizmor_info(),
        license_accepted: config.gsr_zizmor_license_accepted,
        enabled: config.gsr_zizmor_enabled,
        already_installed,
    })
}

/// Called only after the operator has clicked "I accept" on the license
/// dialog the frontend shows using the text from `get_zizmor_info` — this
/// command does not itself display or re-verify that dialog was shown; the
/// dashboard UI is responsible for gating the click that triggers this on
/// the dialog actually having been presented. Persists both the
/// acceptance and the enabled flag together (see `Config::validate`, which
/// refuses `gsr_zizmor_enabled` without `gsr_zizmor_license_accepted`) so
/// this single call leaves config in a state that always passes
/// validation, then attempts the install so a failed download surfaces to
/// the operator immediately rather than silently at the next job.
#[tauri::command]
fn accept_zizmor_license_and_install() -> Result<gitrun_core::InstallOutcome, String> {
    let mut config = load_config()?;
    config.gsr_zizmor_license_accepted = true;

    let outcome = match gitrun_core::ensure_zizmor_installed() {
        Ok(outcome) => outcome,
        Err(error) => {
            config.gsr_zizmor_enabled = false;
            persist_zizmor_config(&config)?;
            return Err(error.to_string());
        }
    };

    config.gsr_zizmor_enabled = matches!(
        outcome,
        gitrun_core::InstallOutcome::AlreadyInstalled | gitrun_core::InstallOutcome::Installed
    );
    config.validate().map_err(|e| e.to_string())?;
    persist_zizmor_config(&config)?;
    Ok(outcome)
}

fn persist_zizmor_config(config: &Config) -> Result<(), String> {
    persist_dashboard_config(config)
}

/// Turns the zizmor integration back off. Deliberately does NOT clear
/// `gsr_zizmor_license_accepted` — the operator already accepted the
/// terms once; re-enabling later shouldn't force them through the dialog
/// again (see the field's doc comment on `Config`). Does not uninstall
/// the binary either: leaving it on disk is harmless and re-enabling
/// later should not require a second download.
#[tauri::command]
fn disable_zizmor() -> Result<(), String> {
    let mut config = load_config()?;
    config.gsr_zizmor_enabled = false;
    persist_dashboard_config(&config)
}

// ---------------------------------------------------------------------
// Logic Containers
// ---------------------------------------------------------------------

#[tauri::command]
fn list_logic_rules() -> Result<Vec<gitrun_scheduler::logic_containers::LogicRule>, String> {
    let config = load_config()?;
    logic_rules_for_dashboard(&config)
}

#[tauri::command]
fn save_logic_rules(
    rules: Vec<gitrun_scheduler::logic_containers::LogicRule>,
) -> Result<(), String> {
    let config = load_config()?;
    gitrun_scheduler::logic_containers::validate_rules(&rules)
        .map_err(|error| error.to_string())?;
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        return persist_logic_rules_as_root(&rules);
    }

    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    gitrun_scheduler::logic_containers::save_rules(&rules_path, &rules).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// VM / hypervisor
// ---------------------------------------------------------------------

fn system_vm_configs(config: &Config) -> Result<Vec<gitrun_scheduler::vm::VmConfig>, String> {
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        let raw = run_privileged_cli_helper("--read-vm-configs-root", &[])?;
        return serde_json::from_str(raw.trim())
            .map_err(|error| format!("decode authorized VM configuration: {error}"));
    }
    gitrun_scheduler::vm::load_vm_configs(Path::new(&config.state_dir))
        .map_err(|error| error.to_string())
}

fn persist_vm_configs_for_dashboard(
    config: &Config,
    configs: &[gitrun_scheduler::vm::VmConfig],
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        return persist_vm_configs_as_root(configs);
    }
    gitrun_scheduler::vm::save_vm_configs(Path::new(&config.state_dir), configs)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_vm_configs() -> Result<Vec<gitrun_scheduler::vm::VmConfig>, String> {
    let config = load_config()?;
    system_vm_configs(&config)
}

#[tauri::command]
fn save_vm_config(config_entry: gitrun_scheduler::vm::VmConfig) -> Result<(), String> {
    let config = load_config()?;
    let mut all = system_vm_configs(&config)?;
    all.retain(|vm| !vm.name.eq_ignore_ascii_case(&config_entry.name));
    all.push(config_entry);
    persist_vm_configs_for_dashboard(&config, &all)
}

#[tauri::command]
fn delete_vm_config(name: String) -> Result<(), String> {
    let config = load_config()?;
    let mut all = system_vm_configs(&config)?;
    let original_len = all.len();
    all.retain(|vm| !vm.name.eq_ignore_ascii_case(name.trim()));
    if all.len() == original_len {
        return Err(format!("VM configuration was not found: {name}"));
    }
    persist_vm_configs_for_dashboard(&config, &all)
}

// ---------------------------------------------------------------------
// Hypervisor decisions — the "KVM isn't available for VM '<name>': <error>.
// Retry KVM, or use VirtualBox instead?" prompt. See
// `gitrun_core::hypervisor_decision` for the full protocol this is a thin
// wrapper around, and `gitrun-scheduler::vm_resolution` for the background
// thread on the autoscaler side that writes the request and waits (up to
// 5 minutes) for `respond_hypervisor_decision` to answer it.
// ---------------------------------------------------------------------

#[tauri::command]
fn list_pending_hypervisor_decisions(
) -> Result<Vec<gitrun_core::hypervisor_decision::PendingDecision>, String> {
    let config = load_config()?;
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        let raw = run_privileged_cli_helper("--read-pending-hypervisor-decisions-root", &[])?;
        return serde_json::from_str(raw.trim())
            .map_err(|error| format!("decode authorized hypervisor decisions: {error}"));
    }
    gitrun_core::hypervisor_decision::list_pending(Path::new(&config.state_dir))
        .map_err(|error| error.to_string())
}

/// The operator's answer to a pending prompt. Does *not* itself bring the
/// VM up — it only writes the choice; the autoscaler's background
/// resolution thread (already polling, see `vm_resolution::resolve_blocking`)
/// picks it up within `DECISION_POLL_INTERVAL` (5s) and acts on it. If that
/// thread already gave up (5-minute timeout elapsed and cleared the
/// record) before this call lands, this will return an error — the
/// dashboard should show that as "too late, this VM setup was already
/// abandoned; it'll be retried automatically next time it's needed" rather
/// than a generic failure.
#[tauri::command]
fn respond_hypervisor_decision(
    vm_name: String,
    choice: gitrun_core::hypervisor_decision::DecisionChoice,
) -> Result<(), String> {
    let config = load_config()?;
    #[cfg(target_os = "linux")]
    if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
        return respond_hypervisor_decision_as_root(&vm_name, choice);
    }
    gitrun_core::hypervisor_decision::respond(Path::new(&config.state_dir), &vm_name, choice)
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------
// GitRun API / security settings
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ApiPolicySummary {
    pub api: String,
    pub enabled: bool,
    pub operations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DockRequirementSummary {
    pub file: String,
    pub line: usize,
    pub calling_job: Option<String>,
    pub target_job: String,
    pub operation: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KittyDashboardData {
    settings: KittySettings,
    status: KittyDashboardStatus,
    status_error: Option<String>,
}

#[tauri::command]
fn get_kitty_dashboard_data() -> Result<KittyDashboardData, String> {
    let config = load_config()?;
    let state_dir = Path::new(&config.state_dir);
    #[cfg(target_os = "linux")]
    if KittyRuntime::uses_system_settings_path(state_dir) {
        if trusted_system_cli_path().is_none() {
            return Err(
                "A trusted system-installed GitRun CLI was not found; Kitty dashboard state cannot be read safely.".into(),
            );
        }
        let raw = run_privileged_cli_helper("--read-kitty-dashboard-root", &[])?;
        return serde_json::from_str(raw.trim())
            .map_err(|error| format!("decode authorized Kitty dashboard response: {error}"));
    }
    let settings = gitrun_smart_scheduler::load_settings_for_dashboard(state_dir)
        .map_err(|e| e.to_string())?;
    let (status, status_error) = match gitrun_smart_scheduler::load_dashboard_status(state_dir) {
        Ok(status) => (status, None),
        Err(error) => (
            KittyDashboardStatus {
                schema_version: gitrun_smart_scheduler::SETTINGS_SCHEMA_VERSION,
                generated_at_epoch: 0,
                enabled: settings.enabled,
                telemetry_enabled: settings.telemetry_enabled,
                predictive_analytics_enabled: settings.predictive_analytics_enabled,
                workflow_model_visible: settings.workflow_model_visible,
                predictive_warm_pool_enabled: settings.predictive_warm_pool_enabled,
                shadow_mode: settings.shadow_mode,
                repositories: Vec::new(),
            },
            Some(error.to_string()),
        ),
    };
    Ok(KittyDashboardData {
        settings,
        status,
        status_error,
    })
}

#[tauri::command]
fn save_kitty_settings(settings: KittySettings) -> Result<(), String> {
    let _write_guard = dashboard_settings_write_guard()?;
    settings.validate().map_err(|error| error.to_string())?;
    let config = load_config()?;
    let state_dir = Path::new(&config.state_dir);
    if KittyRuntime::uses_system_settings_path(state_dir) {
        if trusted_system_cli_path().is_none() {
            return Err(
                "Trusted system GitRun CLI was not found; Kitty settings cannot be saved safely."
                    .into(),
            );
        }
        persist_kitty_settings_as_root(&settings)
    } else {
        gitrun_smart_scheduler::save_settings_for_local_state_dir(state_dir, &settings)
            .map_err(|error| error.to_string())
    }
}

fn gitrun_settings_path(config: &Config) -> PathBuf {
    GitRunSettings::path_for_state_dir(&config.state_dir)
}

/// Read security policy as the desktop user when its permissions allow it.
/// System installs may keep the state directory root-only; only that explicit
/// permission failure is retried through the trusted, fixed-purpose root helper.
fn gitrun_settings_for_dashboard(config: &Config) -> Result<GitRunSettings, String> {
    let path = gitrun_settings_path(config);
    match GitRunSettings::load_or_default(&path) {
        Ok(settings) => Ok(settings),
        Err(gitrun_core::settings::SettingsError::Io(error))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            #[cfg(target_os = "linux")]
            if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file()
            {
                let raw = run_privileged_cli_helper("--read-gitrun-settings-root", &[])?;
                return serde_json::from_str(raw.trim()).map_err(|error| {
                    format!("decode authorized GitRun security settings: {error}")
                });
            }
            Err(format!(
                "cannot read GitRun security settings {}: {error}",
                path.display()
            ))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[tauri::command]
fn get_gitrun_settings() -> Result<GitRunSettings, String> {
    let config = load_config()?;
    gitrun_settings_for_dashboard(&config)
}

#[tauri::command]
fn save_gitrun_settings(
    settings: GitRunSettings,
    confirm_socket_opt_out: bool,
) -> Result<(), String> {
    let _write_guard = dashboard_settings_write_guard()?;
    let config = load_config()?;
    if settings.schema_version != gitrun_core::SETTINGS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported GitRun settings schema version {}",
            settings.schema_version
        ));
    }
    if settings.repositories.keys().any(|repo| {
        !config
            .repositories
            .iter()
            .any(|configured| configured == repo)
    }) {
        return Err("GitRun settings contain a repository that is not configured in GitRun".into());
    }
    let current_settings = gitrun_settings_for_dashboard(&config)?;
    let socket_opt_out = settings.repositories.iter().any(|(name, repository)| {
        repository.docker.direct_socket_enabled
            && !current_settings
                .repositories
                .get(name)
                .map(|current| current.docker.direct_socket_enabled)
                .unwrap_or(false)
    });
    if socket_opt_out && !confirm_socket_opt_out {
        return Err(
            "enabling direct Docker socket access requires explicit danger-gate confirmation"
                .into(),
        );
    }
    persist_gitrun_settings(&settings, &gitrun_settings_path(&config))
}

#[tauri::command]
fn get_repo_api_policies(repo: String) -> Result<Vec<ApiPolicySummary>, String> {
    let config = load_config()?;
    let settings = gitrun_settings_for_dashboard(&config)?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == &repo)
    {
        return Err(format!("repository is not configured in GitRun: {repo}"));
    }
    let effective = settings.effective_for_repository(&repo);
    Ok(GitRunApi::ALL
        .into_iter()
        .map(|api| {
            let policy = effective.api_policy.get(api);
            ApiPolicySummary {
                api: api.as_str().into(),
                enabled: policy.enabled,
                operations: policy
                    .allowed_operations
                    .into_iter()
                    .map(|op| op.as_str().into())
                    .collect(),
            }
        })
        .collect())
}

fn load_dock_requirements_for_repo(
    config: &Config,
    repo: &str,
) -> Result<Vec<DockRequirementSummary>, String> {
    if !config
        .repositories
        .iter()
        .any(|configured| configured == repo)
    {
        return Err(format!("repository is not configured in GitRun: {repo}"));
    }

    #[cfg(target_os = "linux")]
    let requests: Vec<gitrun_core::DockRequest> =
        if trusted_system_cli_path().is_some() && Path::new("/etc/gitrun/gitrun.env").is_file() {
            let output =
                run_privileged_cli_helper("--read-dock-requirements-root", &[repo.to_owned()])?;
            serde_json::from_str(output.trim())
                .map_err(|error| format!("decode authorized GitDockRun requirements: {error}"))?
        } else {
            read_dock_requirements_local(config, repo)?
        };

    #[cfg(not(target_os = "linux"))]
    let requests = read_dock_requirements_local(config, repo)?;

    Ok(requests
        .into_iter()
        .map(|request| DockRequirementSummary {
            file: request.file,
            line: request.line,
            calling_job: request.calling_job,
            target_job: request.target_job,
            operation: request.operation.as_str().into(),
        })
        .collect())
}

fn read_dock_requirements_local(
    config: &Config,
    repo: &str,
) -> Result<Vec<gitrun_core::DockRequest>, String> {
    let path = PathBuf::from(&config.state_dir)
        .join("workflow-dock-requirements")
        .join(format!(
            "{}.json",
            gitrun_scheduler::docker::sanitize(repo, '_')
        ));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_str::<Vec<gitrun_core::DockRequest>>(&raw)
        .map_err(|error| format!("decode Dock requirements: {error}"))
}
#[tauri::command]
fn get_dock_requirements(repo: String) -> Result<Vec<DockRequirementSummary>, String> {
    let config = load_config()?;
    load_dock_requirements_for_repo(&config, &repo)
}

#[derive(Debug, Serialize)]
struct RepoDashboardBootstrap {
    detail: RepoDetail,
    requirements: Vec<DockRequirementSummary>,
    settings: GitRunSettings,
    config: Config,
}

/// Loads the repository's local dashboard state from one consistent config
/// snapshot. The frontend previously issued four IPC calls that independently
/// reloaded config and settings before it could render the policy form.
#[tauri::command]
fn get_repo_dashboard_bootstrap(repo: String) -> Result<RepoDashboardBootstrap, String> {
    let config = load_config()?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == &repo)
    {
        return Err(format!("repository is not configured in GitRun: {repo}"));
    }

    let logic_rules = logic_rules_for_dashboard(&config)?;
    let settings = gitrun_settings_for_dashboard(&config)?;
    let docker_policy = settings.effective_for_repository(&repo).docker;
    let detail = RepoDetail {
        vault_groups: config.vault_groups_for_repo(&repo),
        repo: repo.clone(),
        logic_rules,
        docker_policy,
    };
    let requirements = load_dock_requirements_for_repo(&config, &repo)?;

    Ok(RepoDashboardBootstrap {
        detail,
        requirements,
        settings,
        config,
    })
}

// ---------------------------------------------------------------------
// Config (Settings screen)
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SecurityRuntimeSummary {
    pub resource_pressure_enabled: bool,
    pub resource_pressure_cpu_percent: u8,
    pub resource_pressure_memory_percent: u8,
    pub resource_pressure_disk_percent: u8,
    pub resource_pressure_paths: Vec<String>,
    pub runner_network: String,
    pub runner_network_is_dedicated: bool,
    pub shared_cache_scope: String,
    pub seccomp_profile: String,
    pub apparmor_profile: String,
    pub docker_socket_hardening: bool,
}

#[tauri::command]
fn get_security_runtime_summary() -> Result<SecurityRuntimeSummary, String> {
    let config = load_config()?;
    let network = config.runner_network.trim().to_owned();
    let network_lower = network.to_ascii_lowercase();
    Ok(SecurityRuntimeSummary {
        resource_pressure_enabled: config.resource_pressure_enabled,
        resource_pressure_cpu_percent: config.resource_pressure_cpu_percent,
        resource_pressure_memory_percent: config.resource_pressure_memory_percent,
        resource_pressure_disk_percent: config.resource_pressure_disk_percent,
        resource_pressure_paths: config
            .resource_pressure_paths
            .split(';')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect(),
        runner_network_is_dedicated: !network_lower.is_empty()
            && network_lower != "bridge"
            && network_lower != "host"
            && !network_lower.starts_with("container:"),
        runner_network: network,
        shared_cache_scope: config.shared_cache_scope,
        seccomp_profile: config.runner_seccomp_profile,
        apparmor_profile: config.runner_apparmor_profile,
        docker_socket_hardening: config.gsr_docker_socket_hardening,
    })
}

#[tauri::command]
fn get_config() -> Result<Config, String> {
    load_config()
}

fn command_success_with_timeout(program: &str, args: &[&str], timeout: Duration) -> bool {
    let Ok(mut child) = std::process::Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DashboardHealthCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardHealth {
    version: String,
    config_ok: bool,
    config_path: Option<String>,
    docker_ok: bool,
    gsr_process_seen: bool,
    gsr_service_installed: bool,
    gsr_service_active: bool,
    service_installed: bool,
    service_active: bool,
    checks: Vec<DashboardHealthCheck>,
}

#[derive(Clone)]
struct CachedDashboardHealth {
    fetched_at: Instant,
    config_key: u64,
    value: DashboardHealth,
}

static DASHBOARD_HEALTH_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<CachedDashboardHealth>>,
> = std::sync::OnceLock::new();

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ServiceAction {
    Start,
    Stop,
    Restart,
}

fn current_process_is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let values = line.strip_prefix("Uid:")?;
                values.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
        })
        .is_some_and(|effective_uid| effective_uid == 0)
}

fn trusted_systemctl_path() -> Option<PathBuf> {
    [
        PathBuf::from("/usr/bin/systemctl"),
        PathBuf::from("/bin/systemctl"),
    ]
    .into_iter()
    .find_map(|path| trusted_privileged_binary(&path))
}

#[tauri::command]
fn control_gitrun_service(action: ServiceAction) -> Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("GitRun system service controls currently target Linux".into());
    }
    if control_manager_enabled() && !current_process_is_root() {
        let action_arg = match action {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
        };
        let value = control_manager_request(
            "write",
            "service_control",
            serde_json::json!({ "action": action_arg, "confirmed": true }),
        )?;
        return value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Control Manager returned an invalid service-control result".into());
    }
    let systemctl = trusted_systemctl_path()
        .ok_or("trusted systemctl binary was not found at a root-owned system path")?;
    let action_arg = match action {
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
        ServiceAction::Restart => "restart",
    };
    let output = if current_process_is_root() {
        std::process::Command::new(&systemctl)
            .args([action_arg, "gitrun.service"])
            .output()
            .map_err(|error| format!("unable to run systemctl: {error}"))?
    } else {
        let pkexec = pkexec_path()
            .ok_or("trusted pkexec was not found; service operation cannot continue")?;
        std::process::Command::new(pkexec)
            .arg(&systemctl)
            .args([action_arg, "gitrun.service"])
            .output()
            .map_err(|error| {
                format!("unable to request privileged system service control: {error}")
            })?
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!(
                "systemctl {action_arg} gitrun.service exited with {}",
                output.status
            )
        } else {
            format!("systemctl {action_arg} gitrun.service failed: {detail}")
        });
    }
    if let Some(cache) = DASHBOARD_HEALTH_CACHE.get() {
        if let Ok(mut cached) = cache.lock() {
            *cached = None;
        }
    }
    Ok(format!("Requested system service action: {action_arg}"))
}

fn connect_repository_through_manager(repository: String) -> Result<serde_json::Value, String> {
    let repository = repository.trim().to_owned();
    if !valid_setup_repository(&repository) {
        return Err("repository must be in owner/name form and contain only valid GitHub name characters".into());
    }

    let mut config = load_config()?;
    // Verify repository visibility and the API permission required to inspect
    // runner registration before mutating the protected configuration.
    let client = dashboard_github_client(&config)?;
    let mut runners = client
        .list_runners(&repository)
        .map_err(|error| format!("GitHub repository/runner access check failed for {repository}: {error}"))?;

    let already_configured = config
        .repositories
        .iter()
        .any(|configured| configured.eq_ignore_ascii_case(&repository));
    let scheduler_restarted = if already_configured {
        false
    } else {
        config.repositories.push(repository.clone());
        config.validate().map_err(|error| format!("updated repository configuration is invalid: {error}"))?;
        persist_dashboard_config(&config).map_err(|error| {
            format!("repository access was verified, but saving {repository} failed: {error}")
        })?;
        control_gitrun_service(ServiceAction::Restart).map_err(|error| {
            format!("{repository} was saved, but the scheduler could not be restarted: {error}")
        })?;
        let health = get_dashboard_health(Some(true))?;
        if !health.service_active {
            return Err(format!(
                "{repository} was saved, but gitrun.service is not active after the restart"
            ));
        }
        true
    };

    // Runner creation/registration is asynchronous scheduler work. Observe the
    // GitHub inventory for a short bounded window, but report pending honestly
    // rather than claiming a runner exists simply because configuration saved.
    let runner_expected = config.min_runners > 0;
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        let online = runners.iter().filter(|runner| runner.is_online()).count();
        if online > 0 || !runner_expected || attempts >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_secs(4));
        match client.list_runners(&repository) {
            Ok(latest) => runners = latest,
            Err(error) => {
                return Err(format!(
                    "{repository} is configured and the scheduler restarted, but runner verification failed: {error}"
                ));
            }
        }
    }

    let online_count = runners.iter().filter(|runner| runner.is_online()).count();
    let busy_count = runners.iter().filter(|runner| runner.busy).count();
    let runner_names = runners.iter().map(|runner| runner.name.clone()).collect::<Vec<_>>();
    let status = if !scheduler_restarted && already_configured {
        "already_configured"
    } else if !runner_expected {
        "connected_runner_pool_disabled"
    } else if online_count > 0 {
        "connected_runner_online"
    } else {
        "connected_runner_pending"
    };

    Ok(serde_json::json!({
        "repository": repository,
        "repository_access": "verified",
        "configuration_saved": true,
        "already_configured": already_configured,
        "scheduler_restarted": scheduler_restarted,
        "scheduler_active": true,
        "minimum_runners": config.min_runners,
        "runner_inventory_count": runners.len(),
        "online_runner_count": online_count,
        "busy_runner_count": busy_count,
        "runner_names": runner_names,
        "status": status,
        "message": if !runner_expected {
            "Repository is configured; min_runners is zero, so no runner is expected to be started automatically."
        } else if online_count > 0 {
            "Repository access verified, configuration saved, scheduler active, and at least one online GitHub runner observed."
        } else {
            "Repository access verified and scheduler restarted. No online runner was observed during the verification window; inspect scheduler logs and runner provisioning policy."
        }
    }))
}

#[tauri::command]
fn get_dashboard_health(force_refresh: Option<bool>) -> Result<DashboardHealth, String> {
    const CACHE_TTL: Duration = Duration::from_secs(10);
    let config_key = dashboard_config_cache_key();
    let cache = DASHBOARD_HEALTH_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let cached = cache
        .lock()
        .map_err(|_| "dashboard health cache lock poisoned".to_owned())?
        .clone()
        .filter(|snapshot| snapshot.config_key == config_key);
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached
            .as_ref()
            .filter(|snapshot| snapshot.fetched_at.elapsed() < CACHE_TTL)
        {
            return Ok(snapshot.value.clone());
        }
    }

    let config = load_config()?;
    let config_path = configured_config_path();
    let (docker_ok, service_active) = std::thread::scope(|scope| {
        let docker_check = scope
            .spawn(|| command_success_with_timeout("docker", &["info"], Duration::from_secs(5)));
        let service_check = scope.spawn(|| {
            command_success_with_timeout(
                "/usr/bin/systemctl",
                &["is-active", "--quiet", "gitrun.service"],
                Duration::from_secs(3),
            )
        });
        (
            docker_check.join().unwrap_or(false),
            service_check.join().unwrap_or(false),
        )
    });
    let gsr_process_seen = process_name_running("gitrun-gsr");
    let (gsr_service_active, gsr_service_installed) = std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            command_success_with_timeout(
                "/usr/bin/systemctl",
                &["is-active", "--quiet", "gitrun-gsr.service"],
                Duration::from_secs(3),
            )
        });
        let installed = [
            "/etc/systemd/system/gitrun-gsr.service",
            "/usr/lib/systemd/system/gitrun-gsr.service",
            "/lib/systemd/system/gitrun-gsr.service",
        ]
        .iter()
        .any(|path| std::path::Path::new(path).is_file());
        (active.join().unwrap_or(false), installed)
    });
    let service_installed = [
        "/etc/systemd/system/gitrun.service",
        "/usr/lib/systemd/system/gitrun.service",
        "/lib/systemd/system/gitrun.service",
    ]
    .iter()
    .any(|path| std::path::Path::new(path).is_file());
    let vault_status = if config.vault_dir.trim().is_empty() {
        (
            true,
            "GitVault is not configured; this optional feature is disabled.".to_owned(),
        )
    } else if PathBuf::from(&config.vault_dir).is_dir() {
        (true, "Configured vault directory exists. This check does not open the vault or verify secret decryption.".to_owned())
    } else {
        (
            false,
            "Configured vault directory does not exist or is not a directory.".to_owned(),
        )
    };
    let version = std::env::var("GITRUN_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/usr/share/gitrun/version.txt")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| option_env!("GITRUN_BUILD_VERSION").map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    let checks = vec![
        DashboardHealthCheck {
            name: "Configuration".into(),
            ok: true,
            detail: config_path.as_ref().map(|path| format!("Loaded configuration from {}", path.display()))
                .unwrap_or_else(|| "Configuration loaded from the process environment; no persistent path was resolved.".into()),
        },
        DashboardHealthCheck {
            name: "Docker daemon".into(),
            ok: docker_ok,
            detail: if docker_ok { "Docker info command succeeded.".into() } else { "Docker is unavailable or the current user cannot access the daemon.".into() },
        },
        DashboardHealthCheck {
            name: "GSR watchdog service".into(),
            ok: gsr_service_installed && gsr_service_active,
            detail: if gsr_service_active {
                "systemd reports gitrun-gsr.service active.".into()
            } else if gsr_service_installed {
                "The GSR unit exists but systemd does not report it as active.".into()
            } else {
                "The gitrun-gsr.service unit was not found in known systemd locations; run GitRun setup/reinstall to install it.".into()
            },
        },
        DashboardHealthCheck {
            name: "GSR watchdog process".into(),
            ok: gsr_process_seen,
            detail: if gsr_process_seen {
                "The dedicated GSR watchdog process was detected."
            } else {
                "No GSR watchdog process was detected; verify gitrun-gsr.service and its logs."
            }
            .into(),
        },
        DashboardHealthCheck {
            name: "GitVault".into(),
            ok: vault_status.0,
            detail: vault_status.1,
        },
        DashboardHealthCheck {
            name: "GitRun system service".into(),
            ok: service_installed && service_active,
            detail: if service_active {
                "systemd reports gitrun.service active.".into()
            } else if service_installed {
                "The service unit exists but systemd does not report it as active.".into()
            } else {
                "The gitrun.service unit was not found in known systemd locations.".into()
            },
        },
        DashboardHealthCheck {
            name: "Docker socket hardening policy".into(),
            ok: config.gsr_docker_socket_hardening,
            detail: if config.gsr_docker_socket_hardening { "Hardening is enabled in configuration; active runtime application is not verified here.".into() } else { "Hardening is disabled in configuration.".into() },
        },
    ];
    let report = DashboardHealth {
        version,
        config_ok: true,
        config_path: config_path.map(|path| path.display().to_string()),
        docker_ok,
        gsr_process_seen,
        gsr_service_installed,
        gsr_service_active,
        service_installed,
        service_active,
        checks,
    };
    *cache
        .lock()
        .map_err(|_| "dashboard health cache lock poisoned".to_owned())? =
        Some(CachedDashboardHealth {
            fetched_at: Instant::now(),
            config_key,
            value: report.clone(),
        });
    Ok(report)
}

#[derive(Debug, Serialize)]
struct UpdateCheckResult {
    checked: bool,
    available: bool,
    runner_image_update_available: Option<bool>,
    runner_image_status: Option<String>,
    title: String,
    detail: String,
    output: String,
    error: Option<String>,
}

#[tauri::command]
fn check_gitrun_updates() -> UpdateCheckResult {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("GITRUN_BINARY_PATH") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(PathBuf::from("/usr/local/bin/gitrun"));
    candidates.push(PathBuf::from("/usr/bin/gitrun"));
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(if cfg!(windows) {
                "gitrun.exe"
            } else {
                "gitrun"
            }));
        }
    }
    let Some(binary) = candidates
        .into_iter()
        .find_map(|path| trusted_privileged_binary(&path))
    else {
        return UpdateCheckResult {
            checked: false,
            available: false,
            runner_image_update_available: None,
            runner_image_status: None,
            title: "Update check unavailable".into(),
            detail: "The trusted GitRun CLI binary could not be resolved.".into(),
            output: String::new(),
            error: Some("GitRun CLI binary not found".into()),
        };
    };
    let output = std::process::Command::new(binary)
        .args(["--no-cat", "update", "--check"])
        .output();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let combined = [stdout.as_str(), stderr.as_str()]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let lower = combined.to_ascii_lowercase();
            let available = lower.lines().any(|line| {
                let line = line.trim();
                line.starts_with("update available:")
                    || line.starts_with("new version available:")
                    || line.starts_with("update is available:")
            });
            let already_current = lower.contains("up to date")
                || lower.contains("already latest")
                || lower.contains("no update available");
            let runner_image_update_available = lower
                .lines()
                .any(|line| line.trim().starts_with("runner image update available:"));
            let runner_image_status = combined
                .lines()
                .find(|line| line.trim().to_ascii_lowercase().starts_with("runner image"))
                .map(|line| line.trim().to_owned());
            let checked = output.status.success();
            UpdateCheckResult {
                checked,
                available: checked && available,
                runner_image_update_available: if checked {
                    Some(runner_image_update_available)
                } else {
                    None
                },
                runner_image_status,
                title: if !checked {
                    "Update check failed".into()
                } else if available {
                    "Update available".into()
                } else if already_current {
                    "GitRun is up to date".into()
                } else {
                    "Update check completed".into()
                },
                detail: if combined.is_empty() {
                    format!(
                        "Command exited with status {} without output.",
                        output.status
                    )
                } else {
                    combined.clone()
                },
                output: combined,
                error: if checked {
                    None
                } else {
                    Some(format!("Command exited with status {}", output.status))
                },
            }
        }
        Err(error) => UpdateCheckResult {
            checked: false,
            available: false,
            runner_image_update_available: None,
            runner_image_status: None,
            title: "Update check failed".into(),
            detail: error.to_string(),
            output: String::new(),
            error: Some(error.to_string()),
        },
    }
}

#[tauri::command]
async fn update_permanent_runners(app: AppHandle) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("privileged runner-image update currently targets Linux x86_64".into());
    }
    let pkexec = pkexec_path()
        .ok_or("trusted pkexec was not found; runner update cannot safely continue")?;
    let cli = dashboard_cli_path(&app).ok_or(
        "trusted GitRun CLI executable was not found; runner update cannot safely continue",
    )?;

    let app_for_update = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let args = [
            "--no-cat".to_owned(),
            "update".to_owned(),
            "--only-containers".to_owned(),
        ];
        match stream_privileged_command(&app_for_update, &pkexec, &cli, &args) {
            Ok(status) if status.success() => {
                emit_first_setup_event(
                    &app_for_update,
                    FIRST_SETUP_TOTAL,
                    "Permanent runner reconciliation completed. Busy runners are left running and will be retried on a later GTUU run.",
                    "system",
                    true,
                    true,
                );
                Ok(())
            }
            Ok(status) => {
                let detail = format!("runner-only update exited with status {status}");
                emit_first_setup_event(&app_for_update, 0, detail.clone(), "system", true, false);
                Err(detail)
            }
            Err(error) => {
                emit_first_setup_event(&app_for_update, 0, error.clone(), "system", true, false);
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| format!("runner update task failed: {error}"))?
}

#[tauri::command]
fn save_dashboard_settings(
    updated: Config,
    settings: GitRunSettings,
    confirm_socket_opt_out: bool,
    expected_config: Config,
    expected_settings: GitRunSettings,
) -> Result<(), String> {
    let _write_guard = dashboard_settings_write_guard()?;
    updated.validate().map_err(|error| error.to_string())?;
    let current_config = load_config()?;
    if settings.schema_version != gitrun_core::SETTINGS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported GitRun settings schema version {}",
            settings.schema_version
        ));
    }
    if settings.repositories.keys().any(|repo| {
        !updated
            .repositories
            .iter()
            .any(|configured| configured == repo)
    }) {
        return Err("GitRun settings contain a repository that is not configured in GitRun".into());
    }
    let settings_path = gitrun_settings_path(&updated);
    let current_settings = gitrun_settings_for_dashboard(&current_config)?;
    match std::fs::symlink_metadata(&settings_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("GitRun settings path must be a regular, non-symlink file".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("unable to inspect GitRun settings file: {error}")),
    }
    if current_config != expected_config {
        return Err("GitRun configuration changed since this screen was loaded. Refresh the view and retry.".into());
    }
    if updated.state_dir != current_config.state_dir {
        return Err("The runtime state directory is setup-managed and cannot be changed from the dashboard settings screen.".into());
    }
    if current_settings != expected_settings {
        return Err("GitRun security settings changed since this screen was loaded. Refresh the view and retry.".into());
    }
    let socket_opt_out = settings.repositories.iter().any(|(name, repository)| {
        repository.docker.direct_socket_enabled
            && !current_settings
                .repositories
                .get(name)
                .map(|current| current.docker.direct_socket_enabled)
                .unwrap_or(false)
    });
    if socket_opt_out && !confirm_socket_opt_out {
        return Err(
            "enabling direct Docker socket access requires explicit danger-gate confirmation"
                .into(),
        );
    }
    // The dashboard may be launched as an unprivileged desktop user while
    // the canonical system config is root-owned. Use the same trusted
    // elevation path as the dedicated config command, rather than attempting
    // a direct write that can never succeed for this installation.
    if let Err(save_error) = persist_dashboard_config_unlocked(&updated) {
        let rollback = persist_dashboard_config_unlocked(&current_config);
        return Err(match rollback {
            Ok(()) => format!(
                "GitRun configuration save failed: {save_error}. Previous configuration was restored."
            ),
            Err(rollback_error) => format!(
                "GitRun configuration save failed: {save_error}; restoring the previous configuration also failed: {rollback_error}"
            ),
        });
    }

    if let Err(save_error) = persist_gitrun_settings(&settings, &settings_path) {
        let config_rollback = persist_dashboard_config_unlocked(&current_config);
        // Writing the previously effective defaults as an explicit settings
        // file is semantically equivalent to restoring an absent file, and it
        // also works when the system state directory is root-owned.
        let settings_rollback = persist_gitrun_settings(&current_settings, &settings_path);
        let rollback_detail = match (config_rollback, settings_rollback) {
            (Ok(()), Ok(())) => "Previous configuration and settings were restored.".to_owned(),
            (config_result, settings_result) => format!(
                "Rollback results: config={:?}; settings={:?}",
                config_result.err(),
                settings_result.err()
            ),
        };
        return Err(format!(
            "GitRun settings could not be saved: {save_error}. {rollback_detail}"
        ));
    }
    Ok(())
}

/// Persists settings the dashboard is allowed to edit. Mirrors the older
/// egui dashboard's approach (see `gitrun-dashboard`'s `update_env_file`):
/// only ever writes a fixed set of managed keys back into the existing env
/// file, leaving unmanaged keys (including ones this Tauri build doesn't
/// know about yet) untouched on disk.
#[tauri::command]
fn save_config(updated: Config) -> Result<(), String> {
    persist_dashboard_config(&updated)
}

/// Stable, Tauri-independent management entry points shared by the desktop UI and
/// the terminal management console. The console deliberately calls the same
/// validated handlers rather than reimplementing persistence or policy checks.
fn management_tui_arg<T: serde::de::DeserializeOwned>(
    args: &serde_json::Value,
    field: &str,
) -> Result<T, String> {
    let value = args
        .get(field)
        .cloned()
        .ok_or_else(|| format!("missing required management argument: {field}"))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid management argument {field}: {error}"))
}

fn management_tui_value<T: serde::Serialize>(value: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(value)
        .map_err(|error| format!("could not encode management response: {error}"))
}

/// Read live dashboard data from its existing command handlers. Args use
/// snake_case field names and JSON so callers do not need private response DTOs.
pub fn management_tui_read(
    action: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match action {
        "first_run" => Ok(serde_json::Value::Bool(is_first_run())),
        "overview" => management_tui_value(get_overview()?),
        "health" => management_tui_value(get_dashboard_health(Some(
            args.get("force_refresh")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ))?),
        "runners" => management_tui_value(get_runner_snapshot(Some(
            args.get("force_refresh")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ))?),
        "host_resources" => management_tui_value(get_host_resource_snapshot(Some(
            args.get("force_refresh")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ))?),
        "repository_activity" => management_tui_value(get_repository_activity(Some(
            args.get("force_refresh")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ))?),
        "repo_detail" => management_tui_value(get_repo_detail(management_tui_arg(&args, "repo")?)?),
        "repo_bootstrap" => management_tui_value(get_repo_dashboard_bootstrap(
            management_tui_arg(&args, "repo")?,
        )?),
        "config" => management_tui_value(get_config()?),
        "gitrun_settings" => management_tui_value(get_gitrun_settings()?),
        "security_summary" => management_tui_value(get_security_runtime_summary()?),
        "vault_secrets" => management_tui_value(list_vault_secrets()?),
        "gsr_status" => management_tui_value(get_gsr_status()?),
        "gsr_events" => {
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(30) as usize;
            management_tui_value(list_gsr_events(Some(limit))?)
        }
        "service_logs" => {
            let source: String = management_tui_arg(&args, "source")?;
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as usize);
            management_tui_value(list_service_logs(source, limit)?)
        }
        "zizmor" => management_tui_value(get_zizmor_info()?),
        "logic_rules" => management_tui_value(list_logic_rules()?),
        "vm_configs" => management_tui_value(list_vm_configs()?),
        "hypervisor_decisions" => management_tui_value(list_pending_hypervisor_decisions()?),
        "kitty" => management_tui_value(get_kitty_dashboard_data()?),
        "repo_api_policies" => {
            management_tui_value(get_repo_api_policies(management_tui_arg(&args, "repo")?)?)
        }
        "dock_requirements" => {
            management_tui_value(get_dock_requirements(management_tui_arg(&args, "repo")?)?)
        }
        "updates" => management_tui_value(check_gitrun_updates()),
        other => Err(format!("unsupported management read action: {other}")),
    }
}

/// Perform a dashboard operation with the exact same validation, privileged
/// helper routing and danger gates used by the graphical dashboard.
pub fn management_tui_write(
    action: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match action {
        "connect_repository" => management_tui_value(connect_repository_through_manager(
            management_tui_arg(&args, "repository")?,
        )?),
        "vm_configs_replace" => {
            let configs: Vec<gitrun_scheduler::vm::VmConfig> =
                management_tui_arg(&args, "configs")?;
            // Reuse the scheduler's loader validation by temporarily using its
            // public save API only in developer installs; system installs use
            // the existing fixed-purpose privileged writer after structural
            // validation of the complete configuration set.
            if control_manager_enabled() {
                persist_vm_configs_as_root(&configs)?;
            } else {
                let config = load_config()?;
                gitrun_scheduler::vm::save_vm_configs(Path::new(&config.state_dir), &configs)
                    .map_err(|error| error.to_string())?;
            }
            Ok(serde_json::json!({ "ok": true }))
        }
        "fast_stop" => {
            let message = fast_stop_workflow_run(
                management_tui_arg(&args, "repository")?,
                management_tui_arg(&args, "run_id")?,
            )?;
            management_tui_value(serde_json::json!({ "message": message }))
        }
        "vault_set" => {
            set_vault_secret(
                management_tui_arg(&args, "name")?,
                management_tui_arg(&args, "value")?,
                management_tui_arg(&args, "scope")?,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "vault_delete" => {
            delete_vault_secret(
                management_tui_arg(&args, "name")?,
                management_tui_arg(&args, "scope")?,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "zizmor_install" => management_tui_value(accept_zizmor_license_and_install()?),
        "zizmor_disable" => {
            disable_zizmor()?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "logic_rules_save" => {
            save_logic_rules(management_tui_arg(&args, "rules")?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "vm_save" => {
            save_vm_config(management_tui_arg(&args, "config_entry")?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "vm_delete" => {
            delete_vm_config(management_tui_arg(&args, "name")?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "hypervisor_respond" => {
            respond_hypervisor_decision(
                management_tui_arg(&args, "vm_name")?,
                management_tui_arg(&args, "choice")?,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "kitty_save" => {
            save_kitty_settings(management_tui_arg(&args, "settings")?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "gitrun_settings_save" => {
            save_gitrun_settings(
                management_tui_arg(&args, "settings")?,
                args.get("confirm_socket_opt_out")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "config_save" => {
            save_config(management_tui_arg(&args, "updated")?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "dashboard_settings_save" => {
            save_dashboard_settings(
                management_tui_arg(&args, "updated")?,
                management_tui_arg(&args, "settings")?,
                args.get("confirm_socket_opt_out")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                management_tui_arg(&args, "expected_config")?,
                management_tui_arg(&args, "expected_settings")?,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "service_control" => management_tui_value(control_gitrun_service(management_tui_arg(
            &args, "action",
        )?)?),
        other => Err(format!("unsupported management write action: {other}")),
    }
}

/// Small bridge module: writing back to the same env-file format
/// `gitrun-setup` without depending on any legacy dashboard crate. Mirrors the MANAGED_CONFIG_KEYS approach documented in that
/// crate.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            is_first_run,
            secure_private_key,
            run_first_setup,
            uninstall_gitrun,
            reinstall_gitrun,
            get_overview,
            get_runner_snapshot,
            fast_stop_workflow_run,
            get_repository_activity,
            get_host_resource_snapshot,
            get_repo_detail,
            get_repo_dashboard_bootstrap,
            list_vault_secrets,
            set_vault_secret,
            delete_vault_secret,
            get_gsr_status,
            list_gsr_events,
            list_service_logs,
            get_zizmor_info,
            accept_zizmor_license_and_install,
            disable_zizmor,
            list_logic_rules,
            save_logic_rules,
            list_vm_configs,
            save_vm_config,
            delete_vm_config,
            list_pending_hypervisor_decisions,
            respond_hypervisor_decision,
            get_config,
            get_security_runtime_summary,
            get_dashboard_health,
            get_kitty_dashboard_data,
            save_kitty_settings,
            control_gitrun_service,
            check_gitrun_updates,
            update_permanent_runners,
            save_dashboard_settings,
            save_config,
            get_gitrun_settings,
            save_gitrun_settings,
            get_repo_api_policies,
            get_dock_requirements,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the GitRun dashboard");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_settings_write_guard_serializes_concurrent_writers() {
        let guard = dashboard_settings_write_guard().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();

        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _worker_guard = dashboard_settings_write_guard().unwrap();
            finished_tx.send(()).unwrap();
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should start");
        assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());

        drop(guard);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer should continue after the lock is released");
        worker.join().unwrap();
    }

    #[test]
    fn dashboard_build_version_matches_workspace_version_txt() {
        let expected = include_str!("../../../../version.txt").trim();
        assert_eq!(
            option_env!("GITRUN_BUILD_VERSION"),
            Some(expected),
            "Tauri build.rs must propagate version.txt into the dashboard binary"
        );
        assert!(!expected.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn setup_request_is_created_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-request-permissions-{}-{nonce}.conf",
            std::process::id()
        ));

        write_setup_request(&path, "GITHUB_TOKEN=test-token\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "GITHUB_TOKEN=test-token\n"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn setup_request_does_not_remove_preexisting_file_on_create_new_failure() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-request-existing-{}-{nonce}.conf",
            std::process::id()
        ));
        std::fs::write(&path, "existing-file\n").unwrap();

        assert!(write_setup_request(&path, "replacement\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing-file\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn setup_progress_marker_is_parsed() {
        let parsed =
            parse_setup_progress_line("[GitRun setup] [5/9] Building gitrun-runner:latest");
        assert_eq!(
            parsed,
            Some((5, "Building gitrun-runner:latest".to_owned()))
        );
        assert!(parse_setup_progress_line("[GitRun setup] [5/8] invalid").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_privileged_binary_accepts_system_cli_and_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let system_cli = std::path::Path::new("/usr/bin/env");
        assert!(
            system_cli.is_file(),
            "Ubuntu CI runner must provide the system env executable"
        );
        assert!(
            trusted_privileged_binary(system_cli).is_some(),
            "root-owned system executable under a protected path should be accepted"
        );

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "gitrun-dashboard-privileged-path-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let link = directory.join("gitrun");
        symlink(system_cli, &link).unwrap();
        assert!(
            trusted_privileged_binary(&link).is_none(),
            "symlink executable must not be used for a privileged transition"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn trusted_executable_mode_accepts_setuid_root_binary_with_owner_write() {
        assert!(trusted_executable_mode(0o4755));
    }

    #[test]
    fn trusted_executable_mode_rejects_group_or_world_writable_binary() {
        assert!(!trusted_executable_mode(0o4775));
        assert!(!trusted_executable_mode(0o4757));
        assert!(!trusted_executable_mode(0o0644));
    }

    #[cfg(unix)]
    #[test]
    fn private_key_permissions_are_normalized_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "gitrun-private-key-permissions-{}-{}.pem",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "test private key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();

        validate_private_key_path(path.to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_config_path_must_exist_and_is_resolved() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gitrun-dashboard-config-path-{}-{nonce}.env",
            std::process::id()
        ));
        assert!(resolve_config_path(Some(path.as_os_str())).is_err());

        std::fs::write(&path, "GITRUN_REPOSITORIES=owner/repo\n").unwrap();
        assert_eq!(
            resolve_config_path(Some(path.as_os_str())).unwrap(),
            Some(path.clone())
        );
        std::fs::remove_file(path).unwrap();
    }
}
