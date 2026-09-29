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

use gitrun_core::Config;
use gitrun_vault::{Scope, Vault};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

fn load_config() -> Result<Config, String> {
    match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => Config::from_env_file(path),
        Err(_) => Config::from_env(),
    }
    .map_err(|e| e.to_string())
}

fn configured_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    [
        PathBuf::from("/etc/gitrun/gitrun.env"),
        PathBuf::from("config/gitrun.env"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

#[tauri::command]
fn is_first_run() -> bool {
    configured_config_path().is_none()
}

fn dashboard_cli_path(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();

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

    #[cfg(unix)]
    {
        candidates.push(PathBuf::from("/usr/bin/gitrun"));
        candidates.push(PathBuf::from("/usr/local/bin/gitrun"));
    }

    #[cfg(windows)]
    {
        candidates.push(PathBuf::from(r"C:\Program Files\GitRun\gitrun.exe"));
    }

    candidates.into_iter().find(|path| path.is_file())
}

#[tauri::command]
fn run_first_setup(app: AppHandle, token: String, repositories: String) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("graphical first-run setup currently targets Linux x86_64".into());
    }

    let token = token.trim();
    let repositories = repositories
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .collect::<Vec<_>>();

    if token.is_empty() {
        return Err("GitHub token is required".into());
    }
    if token.chars().any(|c| c == '\n' || c == '\r') {
        return Err("GitHub token must not contain newlines".into());
    }
    if repositories.is_empty()
        || repositories.iter().any(|repo| {
            let mut parts = repo.split('/');
            let owner = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default();
            repo.contains('\n')
                || repo.contains('\r')
                || owner.is_empty()
                || name.is_empty()
                || parts.next().is_some()
        })
    {
        return Err("Enter at least one repository as owner/repository".into());
    }

    if std::process::Command::new("pkexec")
        .arg("--version")
        .output()
        .is_err()
    {
        return Err(
            "pkexec is required for the graphical first-run setup. Run GitRun from a desktop Linux session with PolicyKit enabled."
                .into(),
        );
    }

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

    let payload = format!("AUTH_MODE=pat\nGITHUB_TOKEN={}\nGITRUN_REPOSITORIES={}\n", token, repositories.join(","));
    {
        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| e.to_string())?;
            file.write_all(payload.as_bytes())
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, payload.as_bytes()).map_err(|e| e.to_string())?;
        }
    }

    let result = std::process::Command::new("pkexec")
        .arg(&cli)
        .arg("--install-root")
        .arg(&path)
        .output();

    let _ = std::fs::remove_file(&path);

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Err(if stderr.is_empty() {
                if stdout.is_empty() {
                    format!(
                        "privileged GitRun setup failed with status {}",
                        output.status
                    )
                } else {
                    stdout
                }
            } else {
                stderr
            })
        }
        Err(error) => Err(format!("unable to start privileged GitRun setup: {error}")),
    }
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
    pub recent_critical_events: u32,
}

#[tauri::command]
fn get_overview() -> Result<OverviewData, String> {
    let config = load_config()?;
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let recent_critical_events = gitrun_gsr::events::read_all(&events_path)
        .map(|events| {
            events
                .iter()
                .filter(|e| e.severity == gitrun_gsr::Severity::Critical)
                .count() as u32
        })
        .unwrap_or(0);
    let gsr_watching = PathBuf::from(&config.state_dir)
        .join("gitrun-autoscaler.pid")
        .exists();

    Ok(OverviewData {
        repositories: config.repositories.clone(),
        min_runners: config.min_runners,
        max_runners: config.max_runners,
        vault_enabled: !config.vault_dir.trim().is_empty(),
        gsr_watching,
        recent_critical_events,
    })
}

// ---------------------------------------------------------------------
// Per-repo detail (includes Logic Containers rules for that repo, per the
// operator's direction that Logic Containers lives under the repo tab, not
// as a standalone top-level section)
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RepoDetail {
    pub repo: String,
    pub vault_groups: Vec<String>,
    pub logic_rules: Vec<gitrun_scheduler::logic_containers::LogicRule>,
}

#[tauri::command]
fn get_repo_detail(repo: String) -> Result<RepoDetail, String> {
    let config = load_config()?;
    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    let all_rules =
        gitrun_scheduler::logic_containers::load_rules(&rules_path).map_err(|e| e.to_string())?;
    Ok(RepoDetail {
        vault_groups: config.vault_groups_for_repo(&repo),
        repo,
        logic_rules: all_rules,
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
    Vault::open(&config.vault_dir).map_err(|e| e.to_string())
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
    let pid_file = PathBuf::from(&config.state_dir).join("gitrun-autoscaler.pid");
    let watched_pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    Ok(GsrStatus {
        watching: pid_file.exists(),
        watched_pid,
    })
}

#[tauri::command]
fn list_gsr_events(limit: Option<usize>) -> Result<Vec<gitrun_gsr::SecurityEvent>, String> {
    let config = load_config()?;
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let mut events = gitrun_gsr::events::read_all(&events_path).map_err(|e| e.to_string())?;
    events.reverse(); // most recent first
    if let Some(limit) = limit {
        events.truncate(limit);
    }
    Ok(events)
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
    let already_installed = std::process::Command::new("zizmor")
        .arg("--version")
        .output()
        .is_ok();
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
    config.gsr_zizmor_enabled = true;
    config.validate().map_err(|e| e.to_string())?;
    let path = match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            return Err(
                "GITRUN_CONFIG_FILE is not set; cannot determine which file to save to".into(),
            )
        }
    };
    gitrun_setup::update_env_file(&path, &config).map_err(|e| e.to_string())?;
    gitrun_core::ensure_zizmor_installed().map_err(|e| e.to_string())
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
    let path = match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            return Err(
                "GITRUN_CONFIG_FILE is not set; cannot determine which file to save to".into(),
            )
        }
    };
    gitrun_setup_bridge::update_env_file(&path, &config).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Logic Containers
// ---------------------------------------------------------------------

#[tauri::command]
fn list_logic_rules() -> Result<Vec<gitrun_scheduler::logic_containers::LogicRule>, String> {
    let config = load_config()?;
    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    gitrun_scheduler::logic_containers::load_rules(&rules_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_logic_rules(
    rules: Vec<gitrun_scheduler::logic_containers::LogicRule>,
) -> Result<(), String> {
    let config = load_config()?;
    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    gitrun_scheduler::logic_containers::save_rules(&rules_path, &rules).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// VM / hypervisor
// ---------------------------------------------------------------------

#[tauri::command]
fn list_vm_configs() -> Result<Vec<gitrun_scheduler::vm::VmConfig>, String> {
    let config = load_config()?;
    let path = PathBuf::from(&config.state_dir).join("vm-configs.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

#[tauri::command]
fn save_vm_config(config_entry: gitrun_scheduler::vm::VmConfig) -> Result<(), String> {
    let config = load_config()?;
    let path = PathBuf::from(&config.state_dir).join("vm-configs.json");
    let mut all: Vec<gitrun_scheduler::vm::VmConfig> = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| e.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.to_string()),
    };
    all.retain(|vm| vm.name != config_entry.name);
    all.push(config_entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
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
    gitrun_core::hypervisor_decision::list_pending(std::path::Path::new(&config.state_dir))
        .map_err(|e| e.to_string())
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
    gitrun_core::hypervisor_decision::respond(
        std::path::Path::new(&config.state_dir),
        &vm_name,
        choice,
    )
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Config (Settings screen)
// ---------------------------------------------------------------------

#[tauri::command]
fn get_config() -> Result<Config, String> {
    load_config()
}

/// Persists settings the dashboard is allowed to edit. Mirrors the older
/// egui dashboard's approach (see `gitrun-dashboard`'s `update_env_file`):
/// only ever writes a fixed set of managed keys back into the existing env
/// file, leaving unmanaged keys (including ones this Tauri build doesn't
/// know about yet) untouched on disk.
#[tauri::command]
fn save_config(updated: Config) -> Result<(), String> {
    updated.validate().map_err(|e| e.to_string())?;
    let path = match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            return Err(
                "GITRUN_CONFIG_FILE is not set; cannot determine which file to save to".into(),
            )
        }
    };
    gitrun_setup_bridge::update_env_file(&path, &updated).map_err(|e| e.to_string())
}

/// Small bridge module: writing back to the same env-file format
/// `gitrun-setup` without depending on any legacy dashboard crate. Mirrors the MANAGED_CONFIG_KEYS approach documented in that
/// crate.
mod gitrun_setup_bridge {
    use gitrun_core::Config;
    use std::collections::BTreeSet;
    use std::path::Path;

    const MANAGED_CONFIG_KEYS: &[&str] = &[
        "GITRUN_REPOSITORIES",
        "GITRUN_MIN_RUNNERS",
        "GITRUN_MAX_RUNNERS",
        "GITRUN_IDLE_TIMEOUT",
        "GITRUN_POLL_INTERVAL",
        "GITRUN_RUNNER_IMAGE",
        "GITRUN_RUNNER_LABELS",
        "GITRUN_EPHEMERAL",
        "GITRUN_STATE_DIR",
        "GITRUN_LOG_DIR",
        "GITRUN_AUTO_CONTAINER_UPDATE",
        "GITRUN_CONTAINER_UPDATE_TIME",
        "GITRUN_AUTO_CONTAINER_RECOVERY",
        "GITRUN_CONTAINER_RECOVERY_COOLDOWN",
        // GSR hardening / command policy / workflow validation. Added
        // alongside the dashboard's GSR settings UI — without these keys
        // here, toggling any of these settings in the dashboard would
        // silently fail to persist (save_config would report success, but
        // gitrun_setup_bridge::update_env_file only ever writes keys in
        // this list, so the change would be lost on the next config load).
        "GITRUN_GSR_DOCKER_SOCKET_HARDENING",
        "GITRUN_GSR_ALLOW_UNSAFE_RUNNER",
        "GITRUN_GSR_COMMAND_POLICY_ENABLED",
        "GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED",
        "GITRUN_GSR_COMMAND_BLACKLIST_ENABLED",
        "GITRUN_GSR_COMMAND_BLACKLIST",
        "GITRUN_GSR_COMMAND_WHITELIST_ENABLED",
        "GITRUN_GSR_COMMAND_WHITELIST",
        "GITRUN_GSR_VIOLATION_ACTION",
        "GITRUN_GSR_WORKFLOW_VALIDATION_ENABLED",
        "GITRUN_GSR_ZIZMOR_ENABLED",
        "GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED",
    ];

    fn config_env_values(config: &Config) -> Vec<(&'static str, String)> {
        vec![
            ("GITRUN_REPOSITORIES", config.repositories.join(",")),
            ("GITRUN_MIN_RUNNERS", config.min_runners.to_string()),
            ("GITRUN_MAX_RUNNERS", config.max_runners.to_string()),
            ("GITRUN_IDLE_TIMEOUT", config.idle_timeout.to_string()),
            ("GITRUN_POLL_INTERVAL", config.poll_interval.to_string()),
            ("GITRUN_RUNNER_IMAGE", config.runner_image.clone()),
            ("GITRUN_RUNNER_LABELS", config.runner_labels.clone()),
            ("GITRUN_EPHEMERAL", config.ephemeral.to_string()),
            ("GITRUN_STATE_DIR", config.state_dir.clone()),
            ("GITRUN_LOG_DIR", config.log_dir.clone()),
            (
                "GITRUN_AUTO_CONTAINER_UPDATE",
                config.auto_container_update.to_string(),
            ),
            (
                "GITRUN_CONTAINER_UPDATE_TIME",
                config.container_update_time.clone(),
            ),
            (
                "GITRUN_AUTO_CONTAINER_RECOVERY",
                config.auto_container_recovery.to_string(),
            ),
            (
                "GITRUN_CONTAINER_RECOVERY_COOLDOWN",
                config.container_recovery_cooldown.to_string(),
            ),
            (
                "GITRUN_GSR_DOCKER_SOCKET_HARDENING",
                config.gsr_docker_socket_hardening.to_string(),
            ),
            (
                "GITRUN_GSR_ALLOW_UNSAFE_RUNNER",
                config.gsr_allow_unsafe_runner.to_string(),
            ),
            (
                "GITRUN_GSR_COMMAND_POLICY_ENABLED",
                config.gsr_command_policy_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED",
                config.gsr_command_baseline_blacklist_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_COMMAND_BLACKLIST_ENABLED",
                config.gsr_command_blacklist_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_COMMAND_BLACKLIST",
                config.gsr_command_blacklist.clone(),
            ),
            (
                "GITRUN_GSR_COMMAND_WHITELIST_ENABLED",
                config.gsr_command_whitelist_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_COMMAND_WHITELIST",
                config.gsr_command_whitelist.clone(),
            ),
            (
                "GITRUN_GSR_VIOLATION_ACTION",
                config.gsr_violation_action.clone(),
            ),
            (
                "GITRUN_GSR_WORKFLOW_VALIDATION_ENABLED",
                config.gsr_workflow_validation_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_ZIZMOR_ENABLED",
                config.gsr_zizmor_enabled.to_string(),
            ),
            (
                "GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED",
                config.gsr_zizmor_license_accepted.to_string(),
            ),
        ]
    }

    pub fn update_env_file(path: &Path, config: &Config) -> Result<(), String> {
        let original = std::fs::read_to_string(path)
            .map_err(|e| format!("unable to read {}: {e}", path.display()))?;
        let values = config_env_values(config);
        let managed: BTreeSet<&str> = MANAGED_CONFIG_KEYS.iter().copied().collect();
        let mut seen = BTreeSet::new();
        let mut output = Vec::new();

        for line in original.lines() {
            let trimmed = line.trim();
            if let Some((key, _)) = trimmed.split_once('=') {
                let key = key.trim();
                if managed.contains(key) {
                    if let Some((_, value)) = values.iter().find(|(k, _)| *k == key) {
                        output.push(format!("{key}={value}"));
                        seen.insert(key);
                        continue;
                    }
                }
            }
            output.push(line.to_owned());
        }
        for (key, value) in &values {
            if !seen.contains(key) {
                output.push(format!("{key}={value}"));
            }
        }

        let tmp = path.with_extension("env.tmp");
        std::fs::write(&tmp, output.join("\n") + "\n").map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            is_first_run,
            run_first_setup,
            get_overview,
            get_repo_detail,
            list_vault_secrets,
            set_vault_secret,
            delete_vault_secret,
            get_gsr_status,
            list_gsr_events,
            get_zizmor_info,
            accept_zizmor_license_and_install,
            disable_zizmor,
            list_logic_rules,
            save_logic_rules,
            list_vm_configs,
            save_vm_config,
            list_pending_hypervisor_decisions,
            respond_hypervisor_decision,
            get_config,
            save_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the GitRun dashboard");
}
