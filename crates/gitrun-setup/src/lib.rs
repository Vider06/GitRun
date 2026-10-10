use gitrun_core::{Config, GitRunSettings};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

mod resources;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyStatus {
    pub name: &'static str,
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupReport {
    pub dependencies: Vec<DependencyStatus>,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapAuth {
    Pat(String),
    GitHubApp {
        app_id: String,
        installation_id: String,
        private_key_path: String,
    },
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error("failed to create setup directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid GitRun configuration: {0}")]
    Config(#[from] gitrun_core::ConfigError),
    #[error("configuration directory must not be a file: {0}")]
    InvalidConfigDir(PathBuf),
    #[error("state/log directories must not be files")]
    InvalidRuntimeDir,
    #[error("invalid installation binary: {0}")]
    InvalidInstallBinary(String),
    #[error("setup requires root privileges")]
    NotRoot,
    #[error("unsupported platform: GitRun's bundled installer currently targets Linux")]
    UnsupportedPlatform,
    #[error("missing required command: {0}")]
    MissingCommand(String),
    #[error("command failed: {0}")]
    Command(String),
    #[error("invalid setup repository: {0}")]
    InvalidRepository(String),
}

pub fn check_dependencies() -> Vec<DependencyStatus> {
    vec![
        command_status("docker", &["--version"]),
        docker_daemon_status(),
        compose_status(),
        command_status("git", &["--version"]),
    ]
}

pub fn prepare_directories(
    config: &Config,
    config_dir: impl Into<PathBuf>,
) -> Result<SetupReport, SetupError> {
    config.validate()?;
    let config_dir = config_dir.into();
    if config_dir.exists() && !config_dir.is_dir() {
        return Err(SetupError::InvalidConfigDir(config_dir));
    }

    let state_dir = PathBuf::from(&config.state_dir);
    let log_dir = PathBuf::from(&config.log_dir);
    ensure_directory(&config_dir, 0o700)?;
    ensure_directory(&state_dir, 0o750)?;
    ensure_directory(&log_dir, 0o750)?;

    let settings_path = GitRunSettings::path_for_state_dir(&state_dir);
    if !settings_path.exists() {
        GitRunSettings::default()
            .save(&settings_path)
            .map_err(|error| SetupError::Io(std::io::Error::other(error.to_string())))?;
    }

    Ok(SetupReport {
        dependencies: check_dependencies(),
        config_dir,
        state_dir,
        log_dir,
    })
}

pub fn config_file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("gitrun.env")
}

pub const SETUP_FLAG_PATH: &str = "/etc/gitrun/setup.flag";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationState {
    FirstRun,
    Incomplete,
    Complete,
}

pub fn installation_state() -> InstallationState {
    let config_present = Path::new("/etc/gitrun/gitrun.env").is_file();
    let flag_valid = setup_flag_is_valid();

    match (config_present, flag_valid) {
        (false, false) => InstallationState::FirstRun,
        (true, true) => InstallationState::Complete,
        _ => InstallationState::Incomplete,
    }
}

pub fn setup_flag_path() -> PathBuf {
    PathBuf::from(SETUP_FLAG_PATH)
}

pub fn setup_flag_is_valid() -> bool {
    let path = setup_flag_path();
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        if metadata.uid() != 0 || (metadata.permissions().mode() & 0o777) != 0o600 {
            return false;
        }
    }

    fs::read_to_string(path)
        .ok()
        .is_some_and(|content| parse_setup_flag(&content))
}

fn parse_setup_flag(content: &str) -> bool {
    let Some(value) = content.trim().strip_prefix("GITRUN_SETUP_COMPLETE=") else {
        return false;
    };
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn generate_setup_nonce() -> Result<String, SetupError> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let mut random = fs::File::open("/dev/urandom")?;
    random.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_setup_flag() -> Result<(), SetupError> {
    let nonce = generate_setup_nonce()?;
    let content = format!("GITRUN_SETUP_COMPLETE={nonce}\n");
    write_resource(&setup_flag_path(), &content, 0o600)
}

/// Removes GitRun runtime state and fully purges its Debian package, when present.
/// Only GitRun-owned Docker objects and app-specific user state are removed; Docker
/// itself and other host workloads are shared dependencies and are not purged.
pub fn uninstall_linux() -> Result<(), SetupError> {
    validate_uninstall_host()?;
    let package_registered = deb_package_registered();
    cleanup_uninstall_resources(package_registered)?;

    if package_registered {
        println!("[GitRun uninstall] Purging package registration with dpkg");
        run_command(Command::new("dpkg").args(["--purge", "gitrun"]))?;
    }

    // Catch leftovers from previous manual installs and old package layouts.
    cleanup_residual_paths(false)?;
    cleanup_user_application_data()?;
    cleanup_temp_artifacts()?;
    reload_systemd()?;
    verify_uninstall_complete()?;
    println!("GitRun uninstall: PASS — GitRun-owned host artifacts removed");
    Ok(())
}

/// Entry point for the .deb maintainer hook. This must not invoke dpkg again:
/// dpkg owns the package files and removes them immediately after this hook exits.
pub fn uninstall_linux_package_hook() -> Result<(), SetupError> {
    validate_uninstall_host()?;
    cleanup_uninstall_resources(true)?;
    cleanup_temp_artifacts()?;
    reload_systemd()?;
    Ok(())
}

fn validate_uninstall_host() -> Result<(), SetupError> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err(SetupError::UnsupportedPlatform);
    }
    if !running_as_root() {
        return Err(SetupError::NotRoot);
    }
    Ok(())
}

fn deb_package_registered() -> bool {
    let Ok(output) = Command::new("dpkg-query")
        .args(["-W", "-f=${db:Status-Status}", "gitrun"])
        .output()
    else {
        return false;
    };
    output.status.success()
        && matches!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "installed" | "config-files" | "unpacked" | "half-installed"
                | "half-configured" | "triggers-awaited" | "triggers-pending"
        )
}

fn cleanup_uninstall_resources(package_owned_files: bool) -> Result<(), SetupError> {
    println!("[GitRun uninstall] Stopping and disabling GitRun systemd units");
    for unit in ["gitrun-gsr.service", "gitrun.service"] {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", unit])
            .status();
        if Command::new("systemctl")
            .args(["is-active", "--quiet", unit])
            .status()
            .is_ok_and(|status| status.success())
        {
            run_command(Command::new("systemctl").args(["stop", unit]))?;
        }
    }

    println!("[GitRun uninstall] Removing GitRun-owned Docker resources");
    remove_gitrun_docker_resources()?;
    cleanup_residual_paths(package_owned_files)?;
    cleanup_user_application_data()?;
    Ok(())
}

fn cleanup_residual_paths(package_owned_files: bool) -> Result<(), SetupError> {
    for path in [
        "/etc/systemd/system/gitrun.service",
        "/etc/systemd/system/gitrun-gsr.service",
        "/usr/lib/systemd/system/gitrun.service",
        "/usr/lib/systemd/system/gitrun-gsr.service",
        "/lib/systemd/system/gitrun.service",
        "/lib/systemd/system/gitrun-gsr.service",
        "/run/systemd/system/gitrun.service",
        "/run/systemd/system/gitrun-gsr.service",
        "/etc/systemd/system/multi-user.target.wants/gitrun.service",
        "/etc/systemd/system/multi-user.target.wants/gitrun-gsr.service",
        "/etc/systemd/system/graphical.target.wants/gitrun.service",
        "/etc/systemd/system/graphical.target.wants/gitrun-gsr.service",
        "/etc/systemd/system/default.target.wants/gitrun.service",
        "/etc/systemd/system/default.target.wants/gitrun-gsr.service",
        "/usr/local/bin/gitrun",
        "/usr/local/bin/gitrun-recovery",
        "/usr/bin/gitrun-recovery",
        "/usr/local/lib/gitrun",
        "/etc/gitrun",
        "/var/lib/gitrun",
        "/var/log/gitrun",
        "/var/cache/gitrun",
        "/opt/gitrun",
        "/usr/local/share/gitrun",
        SETUP_FLAG_PATH,
    ] {
        remove_path_if_present(Path::new(path))?;
    }

    if !package_owned_files {
        for path in [
            "/usr/bin/gitrun",
            "/usr/libexec/gitrun",
            "/usr/share/gitrun",
            "/usr/share/doc/gitrun",
            "/usr/share/applications/gitrun.desktop",
            "/usr/local/share/applications/gitrun.desktop",
            "/usr/share/icons/hicolor/32x32/apps/gitrun.png",
            "/usr/share/icons/hicolor/64x64/apps/gitrun.png",
            "/usr/share/icons/hicolor/128x128/apps/gitrun.png",
            "/usr/share/icons/hicolor/256x256/apps/gitrun.png",
            "/usr/share/icons/hicolor/512x512/apps/gitrun.png",
        ] {
            remove_path_if_present(Path::new(path))?;
        }
    }
    Ok(())
}

fn cleanup_user_application_data() -> Result<(), SetupError> {
    const RELATIVE_PATHS: &[&str] = &[
        ".config/dev.gitrun.dashboard",
        ".config/gitrun",
        ".config/GitRun",
        ".config/autostart/gitrun.desktop",
        ".config/autostart/dev.gitrun.dashboard.desktop",
        ".local/share/dev.gitrun.dashboard",
        ".local/share/gitrun",
        ".local/share/GitRun",
        ".local/share/applications/gitrun.desktop",
        ".local/share/icons/hicolor/32x32/apps/gitrun.png",
        ".local/share/icons/hicolor/64x64/apps/gitrun.png",
        ".local/share/icons/hicolor/128x128/apps/gitrun.png",
        ".local/share/icons/hicolor/256x256/apps/gitrun.png",
        ".local/share/icons/hicolor/512x512/apps/gitrun.png",
        ".cache/dev.gitrun.dashboard",
        ".cache/gitrun",
        ".cache/GitRun",
        ".local/state/dev.gitrun.dashboard",
        ".local/state/gitrun",
    ];

    // Explicitly enumerate local accounts and delete only paths uniquely owned
    // by GitRun. No other user files or general caches are touched.
    let passwd = fs::read_to_string("/etc/passwd")?;
    let mut homes = BTreeSet::new();
    for line in passwd.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() < 7 || fields[5].trim().is_empty() {
            continue;
        }
        if fields[2].parse::<u32>().is_err() {
            continue;
        }
        let home = PathBuf::from(fields[5]);
        if home.is_absolute() && home != Path::new("/") && home.is_dir() {
            homes.insert(home);
        }
    }

    for home in homes {
        for relative in RELATIVE_PATHS {
            remove_path_if_present(&home.join(relative))?;
        }
    }
    Ok(())
}

fn cleanup_temp_artifacts() -> Result<(), SetupError> {
    let temp_root = std::env::temp_dir();
    let entries = match fs::read_dir(&temp_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("gitrun-setup-tauri-")
            || name.starts_with("gitrun-reinstall-")
            || name.starts_with("gitrun-connect-")
        {
            remove_path_if_present(&entry.path())?;
        }
    }
    Ok(())
}

fn reload_systemd() -> Result<(), SetupError> {
    if !command_exists("systemctl") {
        return Ok(());
    }
    run_command(Command::new("systemctl").args(["daemon-reload"]))
}

fn verify_uninstall_complete() -> Result<(), SetupError> {
    for path in [
        "/usr/bin/gitrun",
        "/usr/local/bin/gitrun",
        "/usr/share/gitrun",
        "/usr/share/doc/gitrun",
        "/usr/libexec/gitrun",
        "/etc/gitrun",
        "/var/lib/gitrun",
        "/var/log/gitrun",
        "/var/cache/gitrun",
        "/opt/gitrun",
        "/etc/systemd/system/gitrun.service",
        "/etc/systemd/system/gitrun-gsr.service",
    ] {
        if Path::new(path).exists() {
            return Err(SetupError::Command(format!(
                "uninstall verification failed: {} still exists",
                path
            )));
        }
    }
    Ok(())
}

fn remove_gitrun_docker_resources() -> Result<(), SetupError> {
    if !command_exists("docker") {
        return Ok(());
    }
    let daemon = Command::new("docker").arg("info").output()?;
    if !daemon.status.success() {
        return Err(SetupError::Command(
            "Docker is installed but its daemon is unavailable; start Docker and retry uninstall so GitRun containers and volumes can be removed".into(),
        ));
    }

    remove_docker_items(
        &["ps", "-aq", "--filter", "label=gitrun.runner=true"],
        &["rm", "-f"],
        "containers",
    )?;
    remove_docker_items(
        &["volume", "ls", "-q", "--filter", "label=gitrun.shared=true"],
        &["volume", "rm"],
        "volumes",
    )?;
    remove_docker_items(
        &["network", "ls", "-q", "--filter", "label=gitrun.managed=true"],
        &["network", "rm"],
        "networks",
    )?;

    let mut image_ids = BTreeSet::new();
    for args in [
        vec!["image", "ls", "-q", "--filter", "label=gitrun.managed=true"],
        vec!["image", "ls", "-q", "--filter", "reference=gitrun-runner:*"],
    ] {
        let output = Command::new("docker").args(args).output()?;
        if !output.status.success() {
            return Err(SetupError::Command(format!(
                "unable to enumerate GitRun images: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        image_ids.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        );
    }
    for image in image_ids {
        run_command(Command::new("docker").args(["image", "rm", "-f", &image]))?;
    }
    Ok(())
}

fn remove_docker_items(
    list_args: &[&str],
    remove_prefix: &[&str],
    label: &str,
) -> Result<(), SetupError> {
    let output = Command::new("docker").args(list_args).output()?;
    if !output.status.success() {
        return Err(SetupError::Command(format!(
            "unable to enumerate GitRun Docker {label}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    for id in String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let mut command = Command::new("docker");
        command.args(remove_prefix).arg(id);
        run_command(&mut command)?;
    }
    Ok(())
}

fn remove_path_if_present(path: &Path) -> Result<(), SetupError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Ok(_) => {
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Rebuilds a configured GitRun installation from scratch while preserving
/// the operator's existing configuration and runtime state. Authentication is
/// supplied by the caller so no secret is copied into a temporary privileged
/// request beyond the existing setup flow.
pub fn reinstall_linux_with_auth(
    auth: BootstrapAuth,
    repositories: &str,
    app_binary: &Path,
    owner_uid: Option<u32>,
) -> Result<SetupReport, SetupError> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err(SetupError::UnsupportedPlatform);
    }
    if !running_as_root() {
        return Err(SetupError::NotRoot);
    }

    let config_path = Path::new("/etc/gitrun/gitrun.env");
    if !config_path.is_file() {
        return Err(SetupError::Command(
            "GitRun is not currently installed; use the normal first-run setup instead".into(),
        ));
    }
    let existing_config = fs::read_to_string(config_path)?;

    println!("[GitRun reinstall] Stopping the existing GitRun services");
    let _ = Command::new("systemctl")
        .args(["disable", "--now", "gitrun-gsr.service"])
        .status();
    let _ = Command::new("systemctl")
        .args(["disable", "--now", "gitrun.service"])
        .status();

    remove_gitrun_docker_resources()?;
    for path in [
        "/etc/systemd/system/gitrun.service",
        "/etc/systemd/system/gitrun-gsr.service",
        "/usr/local/bin/gitrun-recovery",
        "/opt/gitrun",
    ] {
        remove_path_if_present(Path::new(path))?;
    }
    let _ = Command::new("docker")
        .args(["image", "rm", "-f", "gitrun-runner:latest"])
        .status();
    let _ = Command::new("systemctl").args(["daemon-reload"]).status();

    let report = bootstrap_linux_with_auth(auth, repositories, app_binary, owner_uid)?;

    write_resource(config_path, &existing_config, 0o600)?;
    if let Some(uid) = owner_uid {
        chown_path(config_path, uid)?;
    }
    run_command(Command::new("systemctl").args(["restart", "gitrun.service"]))?;
    run_command(Command::new("systemctl").args(["is-active", "--quiet", "gitrun.service"]))?;
    run_command(Command::new("systemctl").args(["restart", "gitrun-gsr.service"]))?;
    run_command(Command::new("systemctl").args(["is-active", "--quiet", "gitrun-gsr.service"]))?;

    println!("GitRun reinstall: PASS");
    Ok(report)
}

pub fn bootstrap_linux_with_auth(
    auth: BootstrapAuth,
    repositories: &str,
    app_binary: &Path,
    owner_uid: Option<u32>,
) -> Result<SetupReport, SetupError> {
    bootstrap_linux_with_auth_and_profile(auth, repositories, app_binary, owner_uid, "workbench")
}

pub fn bootstrap_linux_with_auth_and_profile(
    auth: BootstrapAuth,
    repositories: &str,
    app_binary: &Path,
    owner_uid: Option<u32>,
    runner_profile: &str,
) -> Result<SetupReport, SetupError> {
    if !matches!(runner_profile, "minimum" | "workbench") {
        return Err(SetupError::Command(format!(
            "unsupported runner profile: {runner_profile}"
        )));
    }
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err(SetupError::UnsupportedPlatform);
    }
    if !running_as_root() {
        return Err(SetupError::NotRoot);
    }

    let repositories = repositories
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .collect::<Vec<_>>();

    if repositories.is_empty() {
        return Err(SetupError::InvalidRepository(
            "at least one repository is required".into(),
        ));
    }
    for repo in &repositories {
        if !valid_repo(repo) {
            return Err(SetupError::InvalidRepository((*repo).to_owned()));
        }
    }
    validate_bootstrap_auth(&auth)?;

    setup_progress(1, "Checking Docker and system prerequisites");
    ensure_docker()?;
    ensure_buildx()?;

    let config_dir = PathBuf::from("/etc/gitrun");
    let state_dir = PathBuf::from("/var/lib/gitrun");
    let log_dir = PathBuf::from("/var/log/gitrun");
    let root = PathBuf::from("/opt/gitrun");

    setup_progress(2, "Preparing GitRun system directories");
    ensure_directory(&config_dir, 0o755)?;
    ensure_directory(&state_dir, 0o750)?;
    ensure_directory(&log_dir, 0o750)?;
    ensure_directory(&root, 0o755)?;
    let vault_dir = state_dir.join("vault");
    ensure_directory(&vault_dir, 0o700)?;

    let settings_path = GitRunSettings::path_for_state_dir(&state_dir);
    if !settings_path.exists() {
        GitRunSettings::default()
            .save(&settings_path)
            .map_err(|error| SetupError::Io(std::io::Error::other(error.to_string())))?;
    }

    setup_progress(3, "Installing runner, recovery, and service resources");
    let runner_dockerfile = resources::runner_dockerfile_for_profile(runner_profile)?;
    write_resource(
        &root.join("docker/runner/Dockerfile"),
        &runner_dockerfile,
        0o644,
    )?;
    write_resource(
        &root.join("docker/runner/entrypoint.sh"),
        resources::RUNNER_ENTRYPOINT,
        0o755,
    )?;
    for (relative_path, content, mode) in resources::RUNNER_BUILD_FILES {
        write_resource(&root.join(relative_path), content, *mode)?;
    }
    let recovery_source = find_recovery_binary(app_binary);
    if let Some(source) = &recovery_source {
        let installed_recovery = Path::new("/usr/local/bin/gitrun-recovery");
        install_binary(source, installed_recovery, owner_uid)?;
    }

    let service = if recovery_source.is_some() {
        resources::SYSTEMD_SERVICE
    } else {
        resources::SYSTEMD_SERVICE_DIRECT
    };
    write_resource(
        Path::new("/etc/systemd/system/gitrun.service"),
        service,
        0o644,
    )?;
    write_resource(
        Path::new("/etc/systemd/system/gitrun-gsr.service"),
        resources::SYSTEMD_GSR_SERVICE,
        0o644,
    )?;

    setup_progress(4, "Writing GitRun configuration");
    let config_path = config_dir.join("gitrun.env");
    let rendered = render_bootstrap_config(&auth, &repositories.join(","));
    write_resource(&config_path, &rendered, 0o600)?;

    if let Some(uid) = owner_uid {
        chown_path(&config_path, uid)?;
        chown_path(&state_dir, uid)?;
        chown_path(&log_dir, uid)?;
        chown_path(&settings_path, uid)?;
        chown_path(&vault_dir, uid)?;
        add_user_to_docker_group(uid)?;
    }

    setup_progress(5, "Building gitrun-runner:latest");
    build_image(
        "gitrun-runner:latest",
        &root,
        &root.join("docker/runner/Dockerfile"),
    )?;

    setup_progress(6, "Installing GitRun binaries");
    let installed = PathBuf::from("/usr/local/bin/gitrun");
    install_binary(app_binary, &installed, owner_uid)?;

    setup_progress(7, "Finalizing desktop integration");
    write_resource(
        Path::new("/usr/share/applications/gitrun.desktop"),
        "[Desktop Entry]\nType=Application\nName=GitRun\nComment=GitHub Actions runner control plane\nExec=/usr/bin/gitrun dashboard\nTerminal=false\nCategories=Development;System;\n",
        0o644,
    )?;

    // Mark the installation complete before starting the scheduler. The
    // scheduler's recovery preflight requires this flag, so writing it only
    // after systemctl reports the service active creates a bootstrap deadlock:
    // the service cannot start until setup is complete, while setup waits for
    // the service to start before declaring completion.
    setup_progress(8, "Recording completed setup state");
    write_setup_flag()?;
    if !setup_flag_is_valid() {
        return Err(SetupError::Command(
            "setup completion flag was written but could not be verified".into(),
        ));
    }

    setup_progress(9, "Enabling and starting the GitRun service");
    run_command(Command::new("systemctl").args(["daemon-reload"]))?;
    run_command(Command::new("systemctl").args(["enable", "gitrun.service"]))?;
    run_command(Command::new("systemctl").args(["restart", "gitrun.service"]))?;
    run_command(Command::new("systemctl").args(["is-active", "--quiet", "gitrun.service"]))?;
    run_command(Command::new("systemctl").args(["enable", "gitrun-gsr.service"]))?;
    run_command(Command::new("systemctl").args(["restart", "gitrun-gsr.service"]))?;
    run_command(Command::new("systemctl").args(["is-active", "--quiet", "gitrun-gsr.service"]))?;

    Ok(SetupReport {
        dependencies: check_dependencies(),
        config_dir,
        state_dir,
        log_dir,
    })
}

const SETUP_PROGRESS_TOTAL: u8 = 9;

fn setup_progress(step: u8, message: &str) {
    println!("[GitRun setup] [{step}/{SETUP_PROGRESS_TOTAL}] {message}");
    let _ = std::io::stdout().flush();
}

fn find_recovery_binary(app_binary: &Path) -> Option<PathBuf> {
    [
        std::env::var("GITRUN_RECOVERY_BINARY")
            .ok()
            .map(PathBuf::from),
        Some(PathBuf::from("/usr/local/bin/gitrun-recovery")),
        app_binary.parent().map(|parent| {
            parent.join(if cfg!(windows) {
                "gitrun-recovery.exe"
            } else {
                "gitrun-recovery"
            })
        }),
    ]
    .into_iter()
    .flatten()
    .find(|path| path.is_file())
}

fn render_bootstrap_config(auth: &BootstrapAuth, repositories: &str) -> String {
    let auth_lines = match auth {
        BootstrapAuth::Pat(token) => format!("GITHUB_TOKEN={}\n", token.trim()),
        BootstrapAuth::GitHubApp {
            app_id,
            installation_id,
            private_key_path,
        } => format!(
            "GITRUN_GITHUB_APP_ID={}\nGITRUN_GITHUB_APP_INSTALLATION_ID={}\nGITRUN_GITHUB_APP_PRIVATE_KEY_PATH={}\n",
            app_id.trim(),
            installation_id.trim(),
            private_key_path.trim(),
        ),
    };

    format!(
        "{auth_lines}GITRUN_REPOSITORIES={repositories}\nGITRUN_MIN_RUNNERS=3\nGITRUN_MAX_RUNNERS=8\nGITRUN_IDLE_TIMEOUT=120\nGITRUN_POLL_INTERVAL=5\nGITRUN_AUTO_CONTAINER_UPDATE=false\nGITRUN_CONTAINER_UPDATE_TIME=03:00\nGITRUN_RUNNER_IMAGE=gitrun-runner:latest\nGITRUN_VAULT_DIR=/var/lib/gitrun/vault\nGITRUN_RUNNER_LABELS=self-hosted,Linux,X64\nGITRUN_EPHEMERAL=false\nGITRUN_DISABLE_UPDATE=false\nGITRUN_CONTAINER_CPUS=1\nGITRUN_CONTAINER_MEMORY=1g\nGITRUN_CONTAINER_PIDS=1024\nGITRUN_STATE_DIR=/var/lib/gitrun\nGITRUN_LOG_DIR=/var/log/gitrun\nGITRUN_SHARED_CACHE_VOLUME=gitrun-runner-shared\nGITRUN_RUNNER_HOME_SIZE=8g\nGITRUN_RUNNER_HOME_BACKEND=volume\nGITRUN_RUNNER_ROOTFS_READ_ONLY=true\nGITRUN_RUNNER_WINDOWS_HYPERV_ISOLATION=true\nGITRUN_GITHUB_CONNECT_TIMEOUT=5\nGITRUN_GITHUB_REQUEST_TIMEOUT=20\nGITRUN_UPDATE_PUBLIC_KEY_HEX=11cc9eaf0147dada407ffd3497c2cebce17d195e21c06030c1e2e1dc688f41f1\nGITRUN_UPDATE_PUBLIC_KEY_ID=gitrun-release-ed25519-v1\nGITRUN_UPDATE_SIGNATURE_REQUIRED=true\n"
    )
}

fn validate_bootstrap_auth(auth: &BootstrapAuth) -> Result<(), SetupError> {
    match auth {
        BootstrapAuth::Pat(token) => {
            validate_env_value(token, "GITHUB_TOKEN")?;
        }
        BootstrapAuth::GitHubApp {
            app_id,
            installation_id,
            private_key_path,
        } => {
            validate_env_value(app_id, "GITRUN_GITHUB_APP_ID")?;
            validate_env_value(installation_id, "GITRUN_GITHUB_APP_INSTALLATION_ID")?;
            validate_env_value(private_key_path, "GITRUN_GITHUB_APP_PRIVATE_KEY_PATH")?;

            let key_path = Path::new(private_key_path);
            let metadata = fs::symlink_metadata(key_path).map_err(|error| {
                SetupError::Command(format!(
                    "unable to inspect GitHub App private key at {private_key_path}: {error}"
                ))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(SetupError::Command(format!(
                    "GitHub App private key must not be a symbolic link: {private_key_path}"
                )));
            }
            if !metadata.is_file() {
                return Err(SetupError::Command(format!(
                    "GitHub App private key is not a regular file: {private_key_path}"
                )));
            }
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(SetupError::Command(format!(
                    "GitHub App private key must not be group/world accessible (mode {mode:o}); chmod it to 0600"
                )));
            }

            let key = fs::read_to_string(key_path).map_err(|error| {
                SetupError::Command(format!(
                    "unable to read GitHub App private key at {private_key_path}: {error}"
                ))
            })?;
            gitrun_core::AppAuth::new(
                app_id,
                installation_id,
                &key,
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(20),
            )
            .map_err(|error| {
                SetupError::Command(format!("invalid GitHub App authentication data: {error}"))
            })?;
        }
    }
    Ok(())
}

fn validate_env_value(value: &str, key: &str) -> Result<(), SetupError> {
    if value.trim().is_empty() {
        return Err(SetupError::Command(format!("{key} must not be empty")));
    }
    if value
        .chars()
        .any(|character| character == '\n' || character == '\r')
    {
        return Err(SetupError::Command(format!(
            "{key} must not contain newlines"
        )));
    }
    Ok(())
}

fn ensure_docker() -> Result<(), SetupError> {
    if command_exists("docker") && docker_daemon_ready() && compose_ready() {
        return Ok(());
    }

    if !command_exists("apt-get") {
        return Err(SetupError::MissingCommand("apt-get".into()));
    }

    run_command(Command::new("apt-get").args(["update"]))?;
    run_command(Command::new("apt-get").args(["install", "-y", "docker.io", "docker-compose-v2"]))?;
    run_command(Command::new("systemctl").args(["enable", "--now", "docker"]))?;

    if !docker_daemon_ready() || !compose_ready() {
        return Err(SetupError::Command(
            "Docker installation completed but Docker/Compose is not ready".into(),
        ));
    }
    Ok(())
}

fn docker_buildx_ready() -> bool {
    Command::new("docker")
        .args(["buildx", "version"])
        .status()
        .is_ok_and(|status| status.success())
}

/// BuildKit is required by GitRun's runner Dockerfiles (RUN --mount=type=cache).
/// Some distributions ship Docker without the Buildx plugin, so bootstrap it
/// rather than failing later with the legacy-builder error.
fn ensure_buildx() -> Result<(), SetupError> {
    if docker_buildx_ready() {
        return Ok(());
    }

    if !command_exists("apt-get") {
        return Err(SetupError::MissingCommand(
            "docker buildx (BuildKit support)".into(),
        ));
    }

    println!("[GitRun setup] Docker Buildx is missing; installing the distro plugin");
    run_command(Command::new("apt-get").args(["update"]))?;

    let plugin_status = Command::new("apt-get")
        .args(["install", "-y", "docker-buildx-plugin"])
        .status()?;
    if !plugin_status.success() {
        // Ubuntu/Debian may package the same plugin under this name instead.
        run_command(Command::new("apt-get").args(["install", "-y", "docker-buildx"]))?;
    }

    if !docker_buildx_ready() {
        return Err(SetupError::Command(
            "Docker Buildx installation finished, but docker buildx version still fails".into(),
        ));
    }
    Ok(())
}

fn build_image(tag: &str, context: &Path, dockerfile: &Path) -> Result<(), SetupError> {
    // Use Buildx explicitly: docker build may select the legacy builder on
    // otherwise healthy Docker installations, which cannot process cache mounts.
    run_command(
        Command::new("docker")
            .args(["buildx", "build", "--load", "-t", tag, "-f"])
            .arg(dockerfile)
            .arg(context),
    )
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), SetupError> {
    if path.exists() && !path.is_dir() {
        return Err(SetupError::InvalidRuntimeDir);
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn validate_install_binary(path: &Path, owner_uid: Option<u32>) -> Result<PathBuf, SetupError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SetupError::InvalidInstallBinary(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SetupError::InvalidInstallBinary(format!(
            "{} must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(SetupError::InvalidInstallBinary(format!(
            "{} is not a regular file",
            path.display()
        )));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(SetupError::InvalidInstallBinary(format!(
            "{} is group/world-writable (mode {mode:o})",
            path.display()
        )));
    }
    if mode & 0o111 == 0 {
        return Err(SetupError::InvalidInstallBinary(format!(
            "{} is not executable (mode {mode:o})",
            path.display()
        )));
    }

    let uid = metadata.uid();
    if uid != 0 && owner_uid != Some(uid) {
        return Err(SetupError::InvalidInstallBinary(format!(
            "{} is owned by uid {uid}; expected root or the invoking user's uid",
            path.display()
        )));
    }

    path.canonicalize().map_err(|error| {
        SetupError::InvalidInstallBinary(format!("unable to resolve {}: {error}", path.display()))
    })
}

fn install_binary(
    source: &Path,
    destination: &Path,
    owner_uid: Option<u32>,
) -> Result<(), SetupError> {
    let source = validate_install_binary(source, owner_uid)?;

    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() {
            return Err(SetupError::InvalidInstallBinary(format!(
                "installation destination {} must not be a symbolic link",
                destination.display()
            )));
        }
        if !metadata.is_file() {
            return Err(SetupError::InvalidInstallBinary(format!(
                "installation destination {} is not a regular file",
                destination.display()
            )));
        }
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("gitrun");
    let tmp_path = destination.with_file_name(format!(
        "{file_name}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let result = (|| {
        fs::copy(&source, &tmp_path)?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
        fs::File::open(&tmp_path)?.sync_all()?;
        fs::rename(&tmp_path, destination)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn write_resource(path: &Path, content: &str, mode: u32) -> Result<(), SetupError> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open with the final mode set atomically at creation time (subject to umask,
    // which is why we also chmod afterward). This removes the window where a
    // freshly created file — e.g. gitrun.env, which holds the GitHub token in
    // plaintext — sits on disk with default (potentially world/group-readable)
    // permissions before being tightened to 0o600.
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("resource");
    let tmp_path = path.with_file_name(format!(
        "{file_name}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;

        // Belt-and-suspenders: force the exact mode regardless of umask, then
        // atomically move into place so readers never see a partially written file.
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn add_user_to_docker_group(uid: u32) -> Result<(), SetupError> {
    let user = Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|line| line.split(':').next().map(str::to_owned))
        .ok_or_else(|| SetupError::Command(format!("unable to resolve username for uid {uid}")))?;
    run_command(Command::new("usermod").args(["-aG", "docker", &user]))
}

fn chown_path(path: &Path, uid: u32) -> Result<(), SetupError> {
    run_command(Command::new("chown").arg(uid.to_string()).arg(path))
}

fn valid_repo(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn running_as_root() -> bool {
    Command::new("id")
        .args(["-u"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn docker_daemon_ready() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn docker_daemon_status() -> DependencyStatus {
    if docker_daemon_ready() {
        DependencyStatus {
            name: "docker daemon",
            available: true,
            version: None,
        }
    } else {
        DependencyStatus {
            name: "docker daemon",
            available: false,
            version: None,
        }
    }
}

fn compose_ready() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn compose_status() -> DependencyStatus {
    if compose_ready() {
        DependencyStatus {
            name: "docker compose",
            available: true,
            version: Command::new("docker")
                .args(["compose", "version"])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .map(str::to_owned)
                }),
        }
    } else {
        DependencyStatus {
            name: "docker compose",
            available: false,
            version: None,
        }
    }
}

fn command_status(name: &'static str, args: &[&str]) -> DependencyStatus {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => DependencyStatus {
            name,
            available: true,
            version: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned),
        },
        _ => DependencyStatus {
            name,
            available: false,
            version: None,
        },
    }
}

fn run_command(command: &mut Command) -> Result<(), SetupError> {
    let display = format!("{command:?}");
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(SetupError::Command(format!(
            "{display}: process exited with status {status}"
        )))
    }
}

/// Persist the dashboard-managed configuration fields to GitRun's env file.
/// Kept in the setup crate so GUI and CLI use one implementation.
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
    "GITRUN_CONTAINER_CPUS",
    "GITRUN_CONTAINER_MEMORY",
    "GITRUN_CONTAINER_PIDS",
    "GITRUN_DISABLE_UPDATE",
    "GITRUN_SHARED_CACHE_VOLUME",
    "GITRUN_RUNNER_HOME_SIZE",
    "GITRUN_RUNNER_HOME_BACKEND",
    "GITRUN_GITHUB_CONNECT_TIMEOUT",
    "GITRUN_GITHUB_REQUEST_TIMEOUT",
    "GITRUN_VAULT_DIR",
    "GITRUN_VAULT_GROUPS",
    "GITRUN_GTUU_SCHEDULE_TIMEZONE",
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
        ("GITRUN_CONTAINER_CPUS", config.container_cpus.clone()),
        ("GITRUN_CONTAINER_MEMORY", config.container_memory.clone()),
        ("GITRUN_CONTAINER_PIDS", config.container_pids_limit.clone()),
        (
            "GITRUN_DISABLE_UPDATE",
            config.runner_disable_update.to_string(),
        ),
        (
            "GITRUN_SHARED_CACHE_VOLUME",
            config.shared_cache_volume.clone(),
        ),
        ("GITRUN_RUNNER_HOME_SIZE", config.runner_home_size.clone()),
        (
            "GITRUN_RUNNER_HOME_BACKEND",
            config.runner_home_backend.clone(),
        ),
        (
            "GITRUN_GITHUB_CONNECT_TIMEOUT",
            config.github_connect_timeout.to_string(),
        ),
        (
            "GITRUN_GITHUB_REQUEST_TIMEOUT",
            config.github_request_timeout.to_string(),
        ),
        ("GITRUN_VAULT_DIR", config.vault_dir.clone()),
        ("GITRUN_VAULT_GROUPS", config.vault_group_membership.clone()),
        (
            "GITRUN_GTUU_SCHEDULE_TIMEZONE",
            config.gtuu_schedule_timezone.clone(),
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
    config.validate().map_err(|e| e.to_string())?;
    let original =
        fs::read_to_string(path).map_err(|e| format!("unable to read {}: {e}", path.display()))?;
    let values = config_env_values(config);

    // Environment-file values are emitted as one physical line per key.
    // Reject line breaks before writing so a GUI/config value can never
    // inject an additional environment variable or comment into gitrun.env.
    for (key, value) in &values {
        validate_env_file_value(key, value)?;
    }

    let managed: std::collections::BTreeSet<&str> = MANAGED_CONFIG_KEYS.iter().copied().collect();
    let mut seen = std::collections::BTreeSet::new();
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

    // Reuse the same atomic 0600 writer used by bootstrap. In particular,
    // never create the temporary env file with the process umask/default
    // permissions: this file can contain the GitHub PAT.
    write_resource(path, &(output.join("\n") + "\n"), 0o600).map_err(|e| e.to_string())?;
    Ok(())
}

fn validate_env_file_value(key: &str, value: &str) -> Result<(), String> {
    if value.contains('\n') || value.contains('\r') {
        return Err(format!("{key} must not contain newlines"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn setup_flag_parser_accepts_only_the_expected_marker() {
        assert!(parse_setup_flag(
            "GITRUN_SETUP_COMPLETE=0123456789abcdef0123456789abcdef\n"
        ));
        assert!(!parse_setup_flag("GITRUN_SETUP_COMPLETE=not-hex\n"));
        assert!(!parse_setup_flag("OTHER=value\n"));
        assert!(!parse_setup_flag(
            "GITRUN_SETUP_COMPLETE=0123456789abcdef0123456789abcde\n"
        ));
    }

    #[test]
    fn generated_bootstrap_env_parses_and_validates_with_runtime_loader() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-bootstrap-env-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let rendered =
            render_bootstrap_config(&BootstrapAuth::Pat("test-token".into()), "owner/repository");
        assert!(rendered.starts_with("GITHUB_TOKEN=test-token\n"));

        // Every generated GITRUN_* variable must be consumed by the runtime
        // Config parser or explicitly owned by the updater.
        const EXTERNAL_RUNTIME_KEYS: &[&str] = &[
            "GITRUN_UPDATE_PUBLIC_KEY_HEX",
            "GITRUN_UPDATE_PUBLIC_KEY_ID",
            "GITRUN_UPDATE_SIGNATURE_REQUIRED",
        ];
        let parser = include_str!("../../gitrun-core/src/config.rs");
        let consumed_by_config: std::collections::BTreeSet<String> = parser
            .split("get(\"")
            .skip(1)
            .filter_map(|tail| tail.split_once('\"').map(|(key, _)| key.to_owned()))
            .collect();
        let external: std::collections::BTreeSet<&str> =
            EXTERNAL_RUNTIME_KEYS.iter().copied().collect();
        let unowned: Vec<String> = rendered
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, _)| key.trim())
            .filter(|key| key.starts_with("GITRUN_"))
            .filter(|key| !consumed_by_config.contains(*key) && !external.contains(*key))
            .map(str::to_owned)
            .collect();
        assert!(
            unowned.is_empty(),
            "setup-generated environment contains unowned variables: {unowned:?}"
        );
        assert!(!rendered
            .lines()
            .any(|line| line.starts_with("GITRUN_LOG_LEVEL=")));

        fs::write(&path, &rendered).unwrap();

        let config = Config::from_env_file(&path)
            .expect("setup-generated environment must parse through the production loader");
        config
            .validate()
            .expect("setup-generated environment must satisfy production validation");
        assert_eq!(config.repositories, vec!["owner/repository"]);
        assert_eq!(config.vault_dir, "/var/lib/gitrun/vault");
        assert!(config.gsr_docker_socket_hardening);
        assert!(config.runner_rootfs_read_only);
        assert!(rendered.contains("GITRUN_UPDATE_PUBLIC_KEY_HEX="));
        assert!(rendered.contains("GITRUN_UPDATE_PUBLIC_KEY_ID=gitrun-release-ed25519-v1"));
        assert!(rendered.contains("GITRUN_UPDATE_SIGNATURE_REQUIRED=true"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn uninstaller_only_targets_gitrun_owned_docker_resources() {
        let source = include_str!("lib.rs");
        assert!(source.contains("label=gitrun.runner=true"));
        assert!(source.contains("label=gitrun.shared=true"));
        assert!(source.contains("label=gitrun.managed=true"));
        assert!(source.contains("reference=gitrun-runner:*"));
        assert!(!source.contains("apt-get purge docker"));
        assert!(!source.contains("remove_dir_all(Path::new(\"/var/lib/docker\"))"));
    }

    #[test]
    fn package_hook_does_not_recursively_call_dpkg_purge() {
        let source = include_str!("lib.rs");
        let hook = source
            .split("pub fn uninstall_linux_package_hook()")
            .nth(1)
            .unwrap()
            .split("fn validate_uninstall_host()")
            .next()
            .unwrap();
        assert!(!hook.contains("dpkg"));
        assert!(source.contains("args([\"--purge\", \"gitrun\"])"));
    }

    #[test]
    fn config_path_is_inside_config_dir() {
        assert_eq!(
            config_file_path(Path::new("/tmp/gitrun")),
            PathBuf::from("/tmp/gitrun/gitrun.env")
        );
    }

    #[test]
    fn dependency_report_has_required_tools() {
        let names: Vec<_> = check_dependencies().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"docker")
                && names.contains(&"docker daemon")
                && names.contains(&"docker compose")
                && names.contains(&"git")
        );
    }

    #[test]
    fn prepare_directories_rejects_file_as_config_dir() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "not a directory").unwrap();
        let config = Config::default();
        assert!(matches!(
            prepare_directories(&config, &path),
            Err(SetupError::InvalidConfigDir(_))
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn validates_repositories() {
        assert!(valid_repo("Vider06/GitRun"));
        assert!(!valid_repo("bad"));
        assert!(!valid_repo("/repo"));
    }

    #[test]
    fn rejects_newline_in_bootstrap_secret() {
        assert!(validate_env_value("token\nINJECTED=value", "GITHUB_TOKEN").is_err());
    }

    #[test]
    fn update_env_file_preserves_unmanaged_and_updates_managed_values() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-update-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            "GITRUN_MIN_RUNNERS=1\nUNMANAGED=value\nGITRUN_GSR_VIOLATION_ACTION=kill\n",
        )
        .unwrap();

        let config = Config {
            min_runners: 4,
            max_runners: 6,
            gsr_violation_action: "log_only".into(),
            ..Default::default()
        };

        update_env_file(&path, &config).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(content.contains("GITRUN_MIN_RUNNERS=4"));
        assert!(content.contains("GITRUN_MAX_RUNNERS=6"));
        assert!(content.contains("GITRUN_GSR_VIOLATION_ACTION=log_only"));
        assert!(content.contains("UNMANAGED=value"));
        assert!(!content.contains("GITRUN_MIN_RUNNERS=1"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn update_env_file_rejects_newline_in_managed_value() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-update-newline-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "GITRUN_REPOSITORIES=owner/repo\n").unwrap();

        let config = Config {
            runner_labels: "self-hosted\nINJECTED=value".into(),
            ..Default::default()
        };

        let error = update_env_file(&path, &config).unwrap_err();
        assert!(error.contains("GITRUN_RUNNER_LABELS must not contain newlines"));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn update_env_file_writes_config_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-update-mode-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "GITRUN_MIN_RUNNERS=1\n").unwrap();

        let config = Config::default();
        update_env_file(&path, &config).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(path).unwrap();
    }
}
