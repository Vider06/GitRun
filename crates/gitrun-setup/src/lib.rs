use gitrun_core::Config;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
    for path in [&config_dir, &state_dir, &log_dir] {
        if path.exists() && !path.is_dir() {
            return Err(SetupError::InvalidRuntimeDir);
        }
        fs::create_dir_all(path)?;
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

pub fn bootstrap_linux(
    github_token: &str,
    repositories: &str,
    app_binary: &Path,
    owner_uid: Option<u32>,
) -> Result<SetupReport, SetupError> {
    bootstrap_linux_with_auth(
        BootstrapAuth::Pat(github_token.to_owned()),
        repositories,
        app_binary,
        owner_uid,
    )
}

pub fn bootstrap_linux_with_auth(
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

    let repositories = repositories
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .collect::<Vec<_>>();

    if repositories.is_empty() {
        return Err(SetupError::InvalidRepository("at least one repository is required".into()));
    }
    for repo in &repositories {
        if !valid_repo(repo) {
            return Err(SetupError::InvalidRepository((*repo).to_owned()));
        }
    }
    validate_bootstrap_auth(&auth)?;

    ensure_docker()?;

    let config_dir = PathBuf::from("/etc/gitrun");
    let state_dir = PathBuf::from("/var/lib/gitrun");
    let log_dir = PathBuf::from("/var/log/gitrun");
    let root = PathBuf::from("/opt/gitrun");

    for path in [&config_dir, &state_dir, &log_dir, &root] {
        fs::create_dir_all(path)?;
    }

    write_resource(&root.join("autoscaler/gitrun_manager.py"), resources::MANAGER_PY, 0o644)?;
    write_resource(&root.join("autoscaler/gitrun_updater_utility.py"), resources::GTUU_PY, 0o755)?;
    write_resource(&root.join("docker/manager/Dockerfile"), resources::MANAGER_DOCKERFILE, 0o644)?;
    write_resource(&root.join("docker/runner/Dockerfile"), resources::RUNNER_DOCKERFILE, 0o644)?;
    write_resource(&root.join("docker/runner/entrypoint.sh"), resources::RUNNER_ENTRYPOINT, 0o755)?;
    write_resource(&root.join("docker-compose.yml"), resources::COMPOSE_YML, 0o644)?;
    write_resource(Path::new("/etc/systemd/system/gitrun.service"), resources::SYSTEMD_SERVICE, 0o644)?;

    let config_path = config_dir.join("gitrun.env");
    let auth_lines = match &auth {
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
    let rendered = format!(
        "{auth_lines}GITRUN_REPOSITORIES={}\nGITRUN_MIN_RUNNERS=3\nGITRUN_MAX_RUNNERS=8\nGITRUN_IDLE_TIMEOUT=120\nGITRUN_POLL_INTERVAL=5\nGITRUN_AUTO_CONTAINER_UPDATE=false\nGITRUN_CONTAINER_UPDATE_TIME=03:00\nGITRUN_RUNNER_IMAGE=gitrun-runner:latest\nGITRUN_RUNNER_LABELS=self-hosted,Linux,X64\nGITRUN_EPHEMERAL=false\nGITRUN_DISABLE_UPDATE=false\nGITRUN_CONTAINER_CPUS=1\nGITRUN_CONTAINER_MEMORY=1g\nGITRUN_CONTAINER_PIDS=1024\nGITRUN_LOG_LEVEL=INFO\nGITRUN_STATE_DIR=/var/lib/gitrun\nGITRUN_LOG_DIR=/var/log/gitrun\nGITRUN_SHARED_CACHE_VOLUME=gitrun-runner-shared\nGITRUN_RUNNER_HOME_SIZE=8g\nGITRUN_GITHUB_CONNECT_TIMEOUT=5\nGITRUN_GITHUB_REQUEST_TIMEOUT=20\n# GSR and other optional settings use their Config defaults.\n",
        repositories.join(",")
    );
    write_resource(&config_path, &rendered, 0o600)?;

    if let Some(uid) = owner_uid {
        chown_path(&config_path, uid)?;
        chown_path(&state_dir, uid)?;
        chown_path(&log_dir, uid)?;
        add_user_to_docker_group(uid)?;
    }

    build_image("gitrun-manager:latest", &root, &root.join("docker/manager/Dockerfile"))?;
    build_image("gitrun-runner:latest", &root, &root.join("docker/runner/Dockerfile"))?;

    run_command(
        Command::new("docker")
            .args(["compose", "--env-file", "/etc/gitrun/gitrun.env", "-f", "/opt/gitrun/docker-compose.yml", "config", "-q"]),
    )?;
    run_command(
        Command::new("systemctl").args(["daemon-reload"]),
    )?;
    run_command(
        Command::new("systemctl").args(["enable", "gitrun.service"]),
    )?;
    run_command(
        Command::new("systemctl").args(["restart", "gitrun.service"]),
    )?;

    let installed = PathBuf::from("/usr/local/bin/gitrun");
    fs::copy(app_binary, &installed)?;
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))?;

    write_resource(
        Path::new("/usr/share/applications/gitrun.desktop"),
        "[Desktop Entry]\nType=Application\nName=GitRun\nComment=GitHub Actions runner control plane\nExec=/usr/local/bin/gitrun\nTerminal=false\nCategories=Development;System;\n",
        0o644,
    )?;

    Ok(SetupReport {
        dependencies: check_dependencies(),
        config_dir,
        state_dir,
        log_dir,
    })
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

            let key = fs::read_to_string(private_key_path)
                .map_err(|error| SetupError::Command(format!(
                    "unable to read GitHub App private key at {private_key_path}: {error}"
                )))?;
            gitrun_core::AppAuth::new(app_id, installation_id, &key)
                .map_err(|error| SetupError::Command(format!(
                    "invalid GitHub App authentication data: {error}"
                )))?;
        }
    }
    Ok(())
}

fn validate_env_value(value: &str, key: &str) -> Result<(), SetupError> {
    if value.trim().is_empty() {
        return Err(SetupError::Command(format!("{key} must not be empty")));
    }
    if value.chars().any(|character| character == '\n' || character == '\r') {
        return Err(SetupError::Command(format!("{key} must not contain newlines")));
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
    run_command(Command::new("apt-get").args([
        "install",
        "-y",
        "docker.io",
        "docker-compose-v2",
    ]))?;
    run_command(Command::new("systemctl").args(["enable", "--now", "docker"]))?;

    if !docker_daemon_ready() || !compose_ready() {
        return Err(SetupError::Command("Docker installation completed but Docker/Compose is not ready".into()));
    }
    Ok(())
}

fn build_image(tag: &str, context: &Path, dockerfile: &Path) -> Result<(), SetupError> {
    run_command(
        Command::new("docker")
            .args(["build", "-t", tag, "-f"])
            .arg(dockerfile)
            .arg(context),
    )
}

fn write_resource(path: &Path, content: &str, mode: u32) -> Result<(), SetupError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open with the final mode set atomically at creation time (subject to umask,
    // which is why we also chmod afterward). This removes the window where a
    // freshly created file — e.g. gitrun.env, which holds the GitHub token in
    // plaintext — sits on disk with default (potentially world/group-readable)
    // permissions before being tightened to 0o600.
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("resource");
    let tmp_path = path.with_file_name(format!(
        "{file_name}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
    ));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    // Belt-and-suspenders: force the exact mode regardless of umask, then
    // atomically move into place so readers never see a partially written file.
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp_path, path)?;
    Ok(())
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
    run_command(
        Command::new("chown").arg(uid.to_string()).arg(path),
    )
}

fn valid_repo(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && repo.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn running_as_root() -> bool {
    Command::new("id")
        .args(["-u"])
        .output()
        .map(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "0"
        })
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
    Command::new("docker").arg("info").output().map(|o| o.status.success()).unwrap_or(false)
}

fn docker_daemon_status() -> DependencyStatus {
    if docker_daemon_ready() {
        DependencyStatus { name: "docker daemon", available: true, version: None }
    } else {
        DependencyStatus { name: "docker daemon", available: false, version: None }
    }
}

fn compose_ready() -> bool {
    Command::new("docker").args(["compose", "version"]).output().map(|o| o.status.success()).unwrap_or(false)
}

fn compose_status() -> DependencyStatus {
    if compose_ready() {
        DependencyStatus {
            name: "docker compose",
            available: true,
            version: Command::new("docker").args(["compose", "version"]).output().ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_owned)),
        }
    } else {
        DependencyStatus { name: "docker compose", available: false, version: None }
    }
}

fn command_status(name: &'static str, args: &[&str]) -> DependencyStatus {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => DependencyStatus {
            name,
            available: true,
            version: String::from_utf8_lossy(&output.stdout).lines().next().map(str::to_owned),
        },
        _ => DependencyStatus { name, available: false, version: None },
    }
}

fn run_command(command: &mut Command) -> Result<(), SetupError> {
    let display = format!("{command:?}");
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Err(SetupError::Command(if stderr.is_empty() { format!("{display}: {stdout}") } else { format!("{display}: {stderr}") }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};

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
        assert!(names.contains(&"docker") && names.contains(&"docker daemon") && names.contains(&"docker compose") && names.contains(&"git"));
    }

    #[test]
    fn prepare_directories_rejects_file_as_config_dir() {
        let path = std::env::temp_dir().join(format!("gitrun-setup-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::write(&path, "not a directory").unwrap();
        let config = Config::default();
        assert!(matches!(prepare_directories(&config, &path), Err(SetupError::InvalidConfigDir(_))));
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
}
