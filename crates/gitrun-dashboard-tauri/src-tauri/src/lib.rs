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
use gitrun_setup::BootstrapAuth;
use gitrun_vault::{Scope, Vault};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

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
        return path.is_file().then_some(path);
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
            process_has_name(pid, expected_name)
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
    Ok(path)
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
    let result = (|| {
        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| e.to_string())?;
            file.write_all(payload.as_bytes())
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(path, payload.as_bytes()).map_err(|e| e.to_string())?;
        }

        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
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
    let _ = app.emit(
        FIRST_SETUP_EVENT,
        FirstSetupEvent {
            phase: phase.min(FIRST_SETUP_TOTAL),
            total: FIRST_SETUP_TOTAL,
            message: message.into(),
            stream: stream.to_owned(),
            done,
            success,
        },
    );
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
    command.arg(cli).args(args);
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
            &["--uninstall-root".to_owned()],
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirstSetupRequest {
    auth_mode: String,
    token: String,
    repositories: String,
    app_id: String,
    installation_id: String,
    private_key_path: String,
    reinstall: bool,
}

#[tauri::command]
async fn run_first_setup(
    app: AppHandle,
    request: FirstSetupRequest,
) -> Result<(), String> {
    let FirstSetupRequest {
        auth_mode,
        token,
        repositories: repository_input,
        app_id,
        installation_id,
        private_key_path,
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
        "{auth_lines}GITRUN_REPOSITORIES={}\n",
        repositories.join(",")
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

        let _ = std::fs::remove_file(&path);

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
    let cutoff = recent_event_cutoff();
    let recent_critical_events = gitrun_gsr::events::read_all(&events_path)
        .map(|events| {
            events
                .iter()
                .filter(|e| e.severity == gitrun_gsr::Severity::Critical && e.timestamp >= cutoff)
                .count() as u32
        })
        .unwrap_or(0);
    let gsr_watching = process_name_running("gitrun-gsr");

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
    let rules_path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    let all_rules =
        gitrun_scheduler::logic_containers::load_rules(&rules_path).map_err(|e| e.to_string())?;
    let settings = GitRunSettings::load_or_default(gitrun_settings_path(&config))
        .map_err(|e| e.to_string())?;
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
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|pid| process_has_name(*pid, "gitrun-autoscaler"));

    Ok(GsrStatus {
        watching: process_name_running("gitrun-gsr"),
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
        .map(|output| output.status.success())
        .unwrap_or(false);
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
    let path = match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            return Err(
                "GITRUN_CONFIG_FILE is not set; cannot determine which file to save to".into(),
            )
        }
    };
    gitrun_setup::update_env_file(&path, config).map_err(|e| e.to_string())
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
    gitrun_setup::update_env_file(&path, &config).map_err(|e| e.to_string())
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
    gitrun_scheduler::vm::load_vm_configs(std::path::Path::new(&config.state_dir))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn save_vm_config(config_entry: gitrun_scheduler::vm::VmConfig) -> Result<(), String> {
    let config = load_config()?;
    let state_dir = std::path::Path::new(&config.state_dir);
    let mut all = gitrun_scheduler::vm::load_vm_configs(state_dir).map_err(|e| e.to_string())?;
    all.retain(|vm| !vm.name.eq_ignore_ascii_case(&config_entry.name));
    all.push(config_entry);
    gitrun_scheduler::vm::save_vm_configs(state_dir, &all).map_err(|e| e.to_string())
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

fn gitrun_settings_path(config: &Config) -> PathBuf {
    GitRunSettings::path_for_state_dir(&config.state_dir)
}

#[tauri::command]
fn get_gitrun_settings() -> Result<GitRunSettings, String> {
    let config = load_config()?;
    GitRunSettings::load_or_default(gitrun_settings_path(&config)).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_gitrun_settings(
    settings: GitRunSettings,
    confirm_socket_opt_out: bool,
) -> Result<(), String> {
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
    let socket_opt_out = settings
        .repositories
        .values()
        .any(|repo| repo.docker.direct_socket_enabled);
    if socket_opt_out && !confirm_socket_opt_out {
        return Err(
            "enabling direct Docker socket access requires explicit danger-gate confirmation"
                .into(),
        );
    }
    settings
        .save(gitrun_settings_path(&config))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_repo_api_policies(repo: String) -> Result<Vec<ApiPolicySummary>, String> {
    let config = load_config()?;
    let settings = GitRunSettings::load_or_default(gitrun_settings_path(&config))
        .map_err(|e| e.to_string())?;
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

#[tauri::command]
fn get_dock_requirements(repo: String) -> Result<Vec<DockRequirementSummary>, String> {
    let config = load_config()?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == &repo)
    {
        return Err(format!("repository is not configured in GitRun: {repo}"));
    }
    let path = PathBuf::from(&config.state_dir)
        .join("workflow-dock-requirements")
        .join(format!(
            "{}.json",
            gitrun_scheduler::docker::sanitize(&repo, '_')
        ));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_str::<Vec<gitrun_core::DockRequest>>(&raw)
        .map(|requests| {
            requests
                .into_iter()
                .map(|request| DockRequirementSummary {
                    file: request.file,
                    line: request.line,
                    calling_job: request.calling_job,
                    target_job: request.target_job,
                    operation: request.operation.as_str().into(),
                })
                .collect()
        })
        .map_err(|e| format!("decode Dock requirements: {e}"))
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
    gitrun_setup::update_env_file(&path, &updated).map_err(|e| e.to_string())
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
            get_security_runtime_summary,
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
    fn setup_progress_marker_is_parsed() {
        let parsed =
            parse_setup_progress_line("[GitRun setup] [5/9] Building gitrun-runner:latest");
        assert_eq!(
            parsed,
            Some((5, "Building gitrun-runner:latest".to_owned()))
        );
        assert!(parse_setup_progress_line("[GitRun setup] [5/8] invalid").is_none());
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
}
