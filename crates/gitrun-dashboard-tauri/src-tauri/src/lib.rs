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
use gitrun_scheduler::gsr_bridge::VaultToGsrBridge;
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
    pub recent_critical_events: Option<u32>,
    pub warnings: Vec<String>,
}

#[tauri::command]
fn get_overview() -> Result<OverviewData, String> {
    let config = load_config()?;
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let cutoff = recent_event_cutoff();
    let mut warnings = Vec::new();
    let recent_critical_events = match gitrun_gsr::events::read_all(&events_path) {
        Ok(events) => Some(
            events
                .iter()
                .filter(|event| {
                    event.severity == gitrun_gsr::Severity::Critical && event.timestamp >= cutoff
                })
                .count() as u32,
        ),
        Err(error) => {
            warnings.push(format!("Critical-event queue could not be read: {error}"));
            None
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
struct DashboardRunnerSnapshot {
    checked_at: u64,
    stale: bool,
    complete: bool,
    repositories_checked: usize,
    repositories_total: usize,
    workflow_repositories_checked: usize,
    workflow_runs: Vec<DashboardWorkflowRunSummary>,
    runners: Vec<DashboardRunnerRecord>,
    warnings: Vec<String>,
}

#[derive(Clone)]
struct CachedDashboardRunnerSnapshot {
    fetched_at: Instant,
    value: DashboardRunnerSnapshot,
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

#[tauri::command]
fn get_runner_snapshot(force_refresh: Option<bool>) -> Result<DashboardRunnerSnapshot, String> {
    const CACHE_TTL: Duration = Duration::from_secs(60);
    let cache = DASHBOARD_RUNNER_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let cached = cache
        .lock()
        .map_err(|_| "runner snapshot cache lock poisoned".to_owned())?
        .clone();
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached
            .as_ref()
            .filter(|snapshot| snapshot.fetched_at.elapsed() < CACHE_TTL)
        {
            return Ok(snapshot.value.clone());
        }
    }

    let fresh = (|| -> Result<DashboardRunnerSnapshot, String> {
        let config = load_config()?;
        let client = dashboard_github_client(&config)?;
        let mut runners = Vec::new();
        let mut workflow_runs = Vec::new();
        let mut warnings = Vec::new();
        let mut repositories_checked = 0usize;
        let mut workflow_repositories_checked = 0usize;
        for repo in &config.repositories {
            let remote_runners = match client.list_runners(repo) {
                Ok(value) => {
                    repositories_checked += 1;
                    Some(value)
                }
                Err(error) => {
                    warnings.push(format!("{repo}: GitHub runner query failed: {error}"));
                    None
                }
            };
            match client.recent_workflow_run_summary(repo, 100) {
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
                && warnings.is_empty(),
            repositories_checked,
            repositories_total: config.repositories.len(),
            workflow_repositories_checked,
            workflow_runs,
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
                        runners: Vec::new(),
                        warnings: Vec::new(),
                    });
            snapshot.stale = true;
            snapshot
                .warnings
                .push(format!("Runner refresh unavailable: {error}"));
            *cache
                .lock()
                .map_err(|_| "runner snapshot cache lock poisoned".to_owned())? =
                Some(CachedDashboardRunnerSnapshot {
                    fetched_at: Instant::now(),
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
    let cached = cache
        .lock()
        .map_err(|_| "repository activity cache lock poisoned".to_owned())?
        .clone();
    if !force_refresh.unwrap_or(false) {
        if let Some(snapshot) = cached
            .as_ref()
            .filter(|snapshot| snapshot.fetched_at.elapsed() < CACHE_TTL)
        {
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
                .push(format!("Repository activity refresh unavailable: {error}"));
            *cache
                .lock()
                .map_err(|_| "repository activity cache lock poisoned".to_owned())? =
                Some(CachedDashboardRepositoryActivity {
                    fetched_at: Instant::now(),
                    value: snapshot.clone(),
                });
            Ok(snapshot)
        }
    }
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
    if !cfg!(target_os = "linux") {
        return Err("System journal logs are currently available only on Linux.".into());
    }
    let units = journal_units(&source)?;
    let limit = limit.unwrap_or(200).clamp(1, 500);
    let mut command = std::process::Command::new("journalctl");
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
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod journal_log_tests {
    use super::journal_units;

    #[test]
    fn journal_units_are_allowlisted() {
        assert_eq!(journal_units("gsr").unwrap(), vec!["gitrun-gsr.service"]);
        assert_eq!(journal_units("scheduler").unwrap(), vec!["gitrun.service"]);
        assert_eq!(journal_units("all").unwrap(), vec!["gitrun-gsr.service", "gitrun.service"]);
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

#[tauri::command]
fn delete_vm_config(name: String) -> Result<(), String> {
    let config = load_config()?;
    let state_dir = std::path::Path::new(&config.state_dir);
    let mut all = gitrun_scheduler::vm::load_vm_configs(state_dir).map_err(|e| e.to_string())?;
    let original_len = all.len();
    all.retain(|vm| !vm.name.eq_ignore_ascii_case(name.trim()));
    if all.len() == original_len {
        return Err(format!("VM configuration was not found: {name}"));
    }
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
    let current_settings = GitRunSettings::load_or_default(gitrun_settings_path(&config))
        .map_err(|error| error.to_string())?;
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

#[derive(Debug, Serialize)]
struct DashboardHealthCheck {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DashboardHealth {
    version: String,
    config_ok: bool,
    config_path: Option<String>,
    docker_ok: bool,
    gsr_process_seen: bool,
    service_installed: bool,
    service_active: bool,
    checks: Vec<DashboardHealthCheck>,
}

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
    Ok(format!("Requested system service action: {action_arg}"))
}

#[tauri::command]
fn get_dashboard_health() -> Result<DashboardHealth, String> {
    let config = load_config()?;
    let config_path = configured_config_path();
    let docker_ok = command_success_with_timeout("docker", &["info"], Duration::from_secs(5));
    let gsr_process_seen = process_name_running("gitrun-gsr");
    let service_installed = [
        "/etc/systemd/system/gitrun.service",
        "/usr/lib/systemd/system/gitrun.service",
        "/lib/systemd/system/gitrun.service",
    ]
    .iter()
    .any(|path| std::path::Path::new(path).is_file());
    let service_active = command_success_with_timeout(
        "/usr/bin/systemctl",
        &["is-active", "--quiet", "gitrun.service"],
        Duration::from_secs(3),
    );
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
        })
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
            name: "GSR watchdog process".into(),
            ok: gsr_process_seen,
            detail: if gsr_process_seen { "A process named gitrun-gsr was detected; enforcement is not fully verified by this check.".into() } else { "No gitrun-gsr process was detected.".into() },
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
    Ok(DashboardHealth {
        version,
        config_ok: true,
        config_path: config_path.map(|path| path.display().to_string()),
        docker_ok,
        gsr_process_seen,
        service_installed,
        service_active,
        checks,
    })
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
) -> Result<(), String> {
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
    let current_settings = GitRunSettings::load_or_default(gitrun_settings_path(&current_config))
        .map_err(|error| error.to_string())?;
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
    let config_path = configured_config_path()
        .ok_or_else(|| "No persistent GitRun configuration file could be resolved; refusing to save settings to an implicit path".to_owned())?;
    let original_config = std::fs::read(&config_path)
        .map_err(|error| format!("unable to back up configuration before saving: {error}"))?;
    let original_settings = match std::fs::read(&settings_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "unable to back up GitRun settings before saving: {error}"
            ))
        }
    };
    if let Err(error) = gitrun_setup::update_env_file(&config_path, &updated) {
        return match std::fs::write(&config_path, original_config.clone()) {
            Ok(()) => Err(format!("GitRun configuration save failed: {error}. Original configuration restored.")),
            Err(rollback) => Err(format!("GitRun configuration save failed: {error}; restoring the original file also failed: {rollback}")),
        };
    }
    if let Err(save_error) = settings.save(&settings_path) {
        let config_rollback = std::fs::write(&config_path, original_config);
        let settings_rollback = match original_settings {
            Some(bytes) => std::fs::write(&settings_path, bytes).map_err(|error| error.to_string()),
            None => match std::fs::remove_file(&settings_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.to_string()),
            },
        };
        let rollback_detail = match (config_rollback, settings_rollback) {
            (Ok(()), Ok(())) => "Previous files were restored.".to_owned(),
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
            get_runner_snapshot,
            get_repository_activity,
            get_host_resource_snapshot,
            get_repo_detail,
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
