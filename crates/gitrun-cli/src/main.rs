use clap::Parser;
use gitrun_core::{AppAuth, Config, GitHubAuth, Runner};
use gitrun_setup::{bootstrap_linux_with_auth, prepare_directories, BootstrapAuth};
use gitrun_updater::{
    apply_update, build_plan, dependency_status, download_and_verify, fetch_manifest,
    latest_manifest, pin_runner_image, rollback, update_incompatible_dependencies,
    update_runner_image, BackupRecord, UpdatePaths,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn persistent_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        return Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("GITRUN_CONFIG_DIR") {
        let path = PathBuf::from(path).join("gitrun.env");
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

fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    if let Some(path) = persistent_config_path() {
        return Config::from_env_file(path);
    }
    Config::from_env()
}

fn setup_config_dir() -> PathBuf {
    if let Some(path) = persistent_config_path() {
        if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
    }
    if let Ok(path) = std::env::var("GITRUN_CONFIG_DIR") {
        return path.into();
    }
    "config".into()
}

fn current_version() -> String {
    std::env::var("GITRUN_VERSION")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/usr/share/gitrun/version.txt")
                .ok()
                .map(|v| v.trim().to_owned())
        })
        .or_else(|| {
            std::fs::read_to_string("version.txt")
                .ok()
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_else(|| "0.0.0".into())
}

fn target_triple() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".into(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".into(),
        ("windows", "x86_64") => "x86_64-pc-windows-msvc".into(),
        ("macos", "x86_64") => "x86_64-apple-darwin".into(),
        ("macos", "aarch64") => "aarch64-apple-darwin".into(),
        (os, arch) => format!("{arch}-{os}"),
    }
}

fn dependency_snapshot() -> Vec<(String, Option<String>)> {
    [
        ("Git", "git"),
        ("Docker", "docker"),
        ("Node.js", "node"),
        ("Python", "python3"),
    ]
    .into_iter()
    .map(|(name, command)| {
        // Bug fix: dependency_status runs `command` as the actual executable
        // (Command::new(...)), so it must receive the real binary name ("docker"),
        // not the human-readable label ("Docker"). Passing the label meant this
        // check silently never found Docker/Git/Node/Python on Linux, since no
        // such binaries exist under those capitalized/dotted names.
        let status = dependency_status(command, "0.0.0");
        (name.into(), status.installed_version)
    })
    .collect()
}

fn update_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    let manifest = if let Some(url) = args.get(1) {
        fetch_manifest(url)?
    } else {
        latest_manifest(&repository)?
    };
    let current = current_version();
    let target = target_triple();
    let plan = build_plan(&manifest, &current, &target, &dependency_snapshot())?;

    println!(
        "GitRun update: {} -> {}",
        plan.current_version, plan.target_version
    );
    println!("target: {}", plan.target);
    println!("artifact: {}", plan.artifact);
    for dependency in &plan.dependencies {
        println!(
            "{}: {} ({})",
            dependency.name,
            dependency.installed_version.as_deref().unwrap_or("missing"),
            dependency.action
        );
    }

    let updated_dependencies = update_incompatible_dependencies(&plan)?;
    if !updated_dependencies.is_empty() {
        println!("dependencies updated: {}", updated_dependencies.join(", "));
    }

    let work_root = PathBuf::from(
        std::env::var("GITRUN_UPDATE_DIR").unwrap_or_else(|_| ".gitrun-update".into()),
    );
    std::fs::create_dir_all(&work_root)?;
    let archive = work_root.join(&plan.artifact);
    let artifact = manifest.artifact_for(&target)?;
    download_and_verify(&plan.artifact_url, &artifact.sha256, &archive)?;
    println!("checksum: PASS");

    let install_dir =
        PathBuf::from(std::env::var("GITRUN_INSTALL_DIR").unwrap_or_else(|_| "./gitrun".into()));
    let state_dir =
        PathBuf::from(std::env::var("GITRUN_STATE_DIR").unwrap_or_else(|_| "./state".into()));
    let config_dir = std::env::var("GITRUN_CONFIG_DIR").ok().map(PathBuf::from);
    let backup_root =
        PathBuf::from(std::env::var("GITRUN_BACKUP_DIR").unwrap_or_else(|_| "./backups".into()));
    let service_config = std::env::var("GITRUN_SERVICE_CONFIG")
        .ok()
        .map(PathBuf::from);

    let paths = UpdatePaths {
        install_dir,
        state_dir,
        config_dir,
        service_config,
        backup_root,
    };
    let backup = apply_update(&paths, &archive, &target, &manifest.version, true)?;
    if let Some(image) = &plan.runner_image {
        if let Err(error) = update_runner_image(image) {
            rollback(&paths, &backup)?;
            return Err(format!("runner update failed; GitRun was rolled back: {error}").into());
        }
        if let Ok(config_file) = std::env::var("GITRUN_CONFIG_FILE") {
            if let Err(error) = pin_runner_image(config_file, image) {
                rollback(&paths, &backup)?;
                return Err(format!(
                    "runner configuration update failed; GitRun was rolled back: {error}"
                )
                .into());
            }
        }
    }

    if let Err(error) = restart_scheduler_service() {
        rollback(&paths, &backup)?;
        return Err(format!("scheduler restart failed; GitRun was rolled back: {error}").into());
    }

    let version_file = paths.install_dir.join("version.txt");
    std::fs::write(version_file, format!("{}\n", manifest.version))?;
    println!("GitRun update: PASS");
    println!(
        "backup: {}",
        backup
            .install_backup
            .parent()
            .unwrap_or(Path::new("."))
            .display()
    );
    Ok(())
}

fn restart_scheduler_service() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(target_os = "linux") || !Path::new("/etc/systemd/system/gitrun.service").is_file() {
        return Ok(());
    }
    let output = std::process::Command::new("systemctl")
        .args(["restart", "gitrun.service"])
        .output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "systemctl restart gitrun.service failed".into()
        } else {
            detail.into()
        });
    }
    Ok(())
}

fn dashboard_executable() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("GITRUN_DASHBOARD_BINARY") {
        if !path.trim().is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(if cfg!(windows) {
                "gitrun-dashboard-tauri.exe"
            } else {
                "gitrun-dashboard-tauri"
            }));
        }
    }

    #[cfg(unix)]
    {
        candidates.push(PathBuf::from("/usr/bin/gitrun-dashboard-tauri"));
        candidates.push(PathBuf::from("/usr/local/bin/gitrun-dashboard-tauri"));
    }

    #[cfg(windows)]
    {
        candidates.push(PathBuf::from(
            r"C:\Program Files\GitRun\gitrun-dashboard-tauri.exe",
        ));
    }

    candidates.push(PathBuf::from(if cfg!(windows) {
        "gitrun-dashboard-tauri.exe"
    } else {
        "gitrun-dashboard-tauri"
    }));

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            "GitRun Tauri dashboard executable was not found; install the graphical dashboard or set GITRUN_DASHBOARD_BINARY".into()
        })
}

fn recovery_executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("GITRUN_RECOVERY_BINARY") {
        if !path.trim().is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(if cfg!(windows) {
                "gitrun-recovery.exe"
            } else {
                "gitrun-recovery"
            }));
        }
    }
    #[cfg(unix)]
    candidates.push(PathBuf::from("/usr/local/bin/gitrun-recovery"));
    #[cfg(windows)]
    candidates.push(PathBuf::from(
        r"C:\Program Files\GitRun\gitrun-recovery.exe",
    ));
    candidates.into_iter().find(|path| path.is_file())
}

fn dashboard_command() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(recovery) = recovery_executable() {
        let status = std::process::Command::new(recovery)
            .args(["start", "dashboard"])
            .status()?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("GitRun Recovery exited with status {status}").into());
    }

    let executable = dashboard_executable()?;
    let status = std::process::Command::new(executable).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("GitRun dashboard exited with status {status}").into())
    }
}

fn install_root_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let owner_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let executable = std::env::current_exe()?;

    let values = parse_setup_request(&raw)?;
    let repositories = required_setup_value(&values, "GITRUN_REPOSITORIES")?;
    let auth = match required_setup_value(&values, "AUTH_MODE")?.as_str() {
        "pat" => BootstrapAuth::Pat(required_setup_value(&values, "GITHUB_TOKEN")?),
        "app" => BootstrapAuth::GitHubApp {
            app_id: required_setup_value(&values, "GITRUN_GITHUB_APP_ID")?,
            installation_id: required_setup_value(&values, "GITRUN_GITHUB_APP_INSTALLATION_ID")?,
            private_key_path: required_setup_value(&values, "GITRUN_GITHUB_APP_PRIVATE_KEY_PATH")?,
        },
        other => return Err(format!("unsupported setup auth mode: {other}").into()),
    };

    bootstrap_linux_with_auth(auth, &repositories, &executable, owner_uid)?;

    println!("GitRun setup: PASS");
    Ok(())
}

fn parse_setup_request(
    raw: &str,
) -> Result<std::collections::BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let mut values = std::collections::BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("invalid setup request line: {line}"))?;
        if key.trim().is_empty() {
            return Err("setup request contains an empty key".into());
        }
        values.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(values)
}

fn required_setup_value(
    values: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let value = values
        .get(key)
        .cloned()
        .ok_or_else(|| format!("setup request is missing {key}"))?;
    if value.trim().is_empty() {
        return Err(format!("setup request has an empty {key}").into());
    }
    Ok(value)
}

fn terminal_print_header(title: &str) {
    println!();
    println!("╭──────────────────────────────────────────────────────────────╮");
    println!("│ {:<60} │", title);
    println!("╰──────────────────────────────────────────────────────────────╯");
}

fn read_terminal_line(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

fn read_terminal_secret(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    print!("{prompt}");
    io::stdout().flush()?;

    let echo_disabled = std::process::Command::new("stty")
        .arg("-echo")
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    let mut value = String::new();
    let result = io::stdin().read_line(&mut value);

    if echo_disabled {
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
    }

    Ok(result.map(|_| value.trim().to_owned())?)
}

fn read_terminal_choice(prompt: &str, max: usize) -> Result<usize, Box<dyn std::error::Error>> {
    loop {
        let raw = read_terminal_line(prompt)?;
        if let Ok(value) = raw.parse::<usize>() {
            if (1..=max).contains(&value) {
                return Ok(value);
            }
        }
        println!("  Please enter a number from 1 to {max}.");
    }
}

fn verify_repository_access(
    auth: &GitHubAuth,
    repository: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parts = repository.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return Err(format!("invalid repository: {repository}").into());
    }

    let token = auth.bearer_token()?;
    let url = format!("https://api.github.com/repos/{owner}/{name}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2026-03-10")
        .header("User-Agent", concat!("GitRun/", env!("CARGO_PKG_VERSION")))
        .send()?;

    match response.status() {
        reqwest::StatusCode::OK => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED => Err(
            "GitHub rejected the configured credentials (401 Unauthorized).".into()
        ),
        reqwest::StatusCode::FORBIDDEN => Err(
            "GitHub denied access or rate-limited the request (403 Forbidden).".into()
        ),
        reqwest::StatusCode::NOT_FOUND => Err(
            format!("repository {repository} was not found or the configured authentication cannot access it.").into()
        ),
        status => Err(format!(
            "GitHub returned HTTP {} while checking {repository}.",
            status.as_u16()
        ).into()),
    }
}

fn write_repositories_to_config(
    path: &Path,
    repositories: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let original = std::fs::read_to_string(path)?;
    let repository_value = repositories.join(",");
    let mut output = Vec::new();
    let mut replaced = false;

    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.split_once('=').map(|(key, _)| key.trim()) == Some("GITRUN_REPOSITORIES") {
            output.push(format!("GITRUN_REPOSITORIES={repository_value}"));
            replaced = true;
        } else {
            output.push(line.to_owned());
        }
    }

    if !replaced {
        if !output.is_empty() && !output.last().is_some_and(|line| line.is_empty()) {
            output.push(String::new());
        }
        output.push(format!("GITRUN_REPOSITORIES={repository_value}"));
    }

    let mut rendered = output.join("\n");
    rendered.push('\n');

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".gitrun-connect-{}.{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(rendered.as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&temp, rendered.as_bytes())?;
    }

    std::fs::rename(&temp, path)?;
    Ok(())
}

fn terminal_setup_command() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
        return Err("terminal setup currently targets Linux x86_64".into());
    }

    terminal_print_header("GitRun • Terminal Setup");
    println!("Configure GitHub authentication and the repositories GitRun should manage.");
    println!("Credentials are tested before the privileged installation begins.");
    println!();

    let auth_choice = read_terminal_choice(
        "Authentication: [1] Personal Access Token  [2] GitHub App → ",
        2,
    )?;
    let (bootstrap_auth, github_auth) = match auth_choice {
        1 => {
            let token = read_terminal_secret("GitHub Personal Access Token: ")?;
            if token.is_empty() {
                return Err("GitHub token cannot be empty".into());
            }
            let auth = GitHubAuth::Pat(token.clone());
            (BootstrapAuth::Pat(token), auth)
        }
        2 => {
            let app_id = read_terminal_line("GitHub App ID: ")?;
            let installation_id = read_terminal_line("GitHub Installation ID: ")?;
            let private_key_path = read_terminal_line("Private key PEM path: ")?;

            if app_id.is_empty() || installation_id.is_empty() || private_key_path.is_empty() {
                return Err(
                    "GitHub App ID, Installation ID, and private key path are required".into(),
                );
            }

            let private_key = std::fs::read_to_string(&private_key_path)
                .map_err(|error| format!("unable to read private key: {error}"))?;
            let app = AppAuth::new(
                &app_id,
                &installation_id,
                &private_key,
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(20),
            )?;
            let auth = GitHubAuth::App(app);
            // Force the JWT → installation-token exchange now, rather than
            // accepting merely syntactically valid App fields.
            auth.bearer_token()?;

            (
                BootstrapAuth::GitHubApp {
                    app_id,
                    installation_id,
                    private_key_path,
                },
                auth,
            )
        }
        _ => unreachable!(),
    };

    println!();
    let repositories_raw = read_terminal_line("Repositories (owner/repository[,owner/other]): ")?;
    let repositories = repositories_raw
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if repositories.is_empty() {
        return Err("at least one repository is required".into());
    }

    terminal_print_header("Checking GitHub access");
    for repository in &repositories {
        print!("  {repository} ... ");
        io::stdout().flush()?;
        verify_repository_access(&github_auth, repository)?;
        println!("OK");
    }

    let path = std::env::temp_dir().join(format!(
        "gitrun-setup-{}-{}.conf",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));

    let mut payload = String::new();
    match &bootstrap_auth {
        BootstrapAuth::Pat(token) => {
            payload.push_str("AUTH_MODE=pat\n");
            payload.push_str(&format!("GITHUB_TOKEN={token}\n"));
        }
        BootstrapAuth::GitHubApp {
            app_id,
            installation_id,
            private_key_path,
        } => {
            payload.push_str("AUTH_MODE=app\n");
            payload.push_str(&format!("GITRUN_GITHUB_APP_ID={app_id}\n"));
            payload.push_str(&format!(
                "GITRUN_GITHUB_APP_INSTALLATION_ID={installation_id}\n"
            ));
            payload.push_str(&format!(
                "GITRUN_GITHUB_APP_PRIVATE_KEY_PATH={private_key_path}\n"
            ));
        }
    }
    payload.push_str(&format!("GITRUN_REPOSITORIES={}\n", repositories.join(",")));

    // Create the credential-bearing setup request atomically with restrictive
    // permissions. A plain write followed by chmod leaves a brief exposure
    // window in /tmp before the permissions are tightened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(payload.as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, payload.as_bytes())?;
    }

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let executable = std::env::current_exe()?;
        println!();
        println!("Installing GitRun with elevated privileges...");
        let uid = std::process::Command::new("id").arg("-u").output()?;
        let running_as_root =
            uid.status.success() && String::from_utf8_lossy(&uid.stdout).trim() == "0";
        let status = if running_as_root {
            std::process::Command::new(&executable)
                .arg("--install-root")
                .arg(&path)
                .status()?
        } else {
            std::process::Command::new("sudo")
                .arg(&executable)
                .arg("--install-root")
                .arg(&path)
                .status()?
        };

        if !status.success() {
            return Err(format!("GitRun installation failed with status {status}").into());
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&path);
    result
}

fn connect_command(repository: &str) -> Result<(), Box<dyn std::error::Error>> {
    if repository.split('/').count() != 2 || repository.split('/').any(|part| part.is_empty()) {
        return Err("usage: gitrun connect <owner/repository>".into());
    }

    let path = persistent_config_path()
        .ok_or("persistent GitRun configuration not found; run 'gitrun setup --terminal' first")?;
    let config = Config::from_env_file(&path)?;
    let auth = GitHubAuth::from_config_file(&config, &path)?;

    verify_repository_access(&auth, repository)?;

    let mut repositories = config.repositories;
    if repositories
        .iter()
        .any(|repo| repo.eq_ignore_ascii_case(repository))
    {
        println!("Repository already connected: {repository}");
        return Ok(());
    }
    repositories.push(repository.to_owned());
    write_repositories_to_config(&path, &repositories)?;

    println!("Repository connected: {repository}");
    println!("Configuration updated: {}", path.display());
    println!("Restart GitRun to apply it to the scheduler.");
    Ok(())
}

fn rollback_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let backup: BackupRecord = serde_json::from_str(&raw)?;
    let install_dir =
        PathBuf::from(std::env::var("GITRUN_INSTALL_DIR").unwrap_or_else(|_| "./gitrun".into()));
    let state_dir =
        PathBuf::from(std::env::var("GITRUN_STATE_DIR").unwrap_or_else(|_| "./state".into()));
    let config_dir = std::env::var("GITRUN_CONFIG_DIR").ok().map(PathBuf::from);
    let backup_root = backup
        .install_backup
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./backups"));
    let service_config = std::env::var("GITRUN_SERVICE_CONFIG")
        .ok()
        .map(PathBuf::from);
    let paths = UpdatePaths {
        install_dir,
        state_dir,
        config_dir,
        service_config,
        backup_root,
    };
    rollback(&paths, &backup)?;
    println!("GitRun rollback: PASS");
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    if cli.version {
        println!("gitrun {}", current_version());
        std::process::exit(0);
    }
    let exit_code = match cli.command.unwrap_or(Command::Dashboard) {
        Command::Config => run_config(),
        Command::Desired {
            min,
            max,
            busy,
            queued,
        } => run_desired(min, max, busy, queued),
        Command::Setup { terminal } => {
            if terminal {
                match terminal_setup_command() {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("GitRun terminal setup: FAIL — {error}");
                        1
                    }
                }
            } else {
                run_setup()
            }
        }
        Command::Connect { repository } => match connect_command(&repository) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun connect: FAIL — {error}");
                1
            }
        },
        Command::Doctor => run_doctor(),
        Command::Update {
            manifest_url,
            only_containers,
        } => run_update(manifest_url.as_deref(), only_containers),
        Command::Scheduler => {
            gitrun_scheduler::run();
            0
        }
        Command::Dashboard => run_dashboard(),
        Command::InstallRoot { token_path } => run_install_root(&token_path),
        Command::Rollback { backup_path } => run_rollback(&backup_path),
    };
    std::process::exit(exit_code);
}

/// GitRun: self-hosted GitHub Actions runner manager.
#[derive(clap::Parser)]
#[command(name = "gitrun", about, long_about = None, disable_version_flag = true)]
struct Cli {
    /// Print the GitRun version and exit. Accepts both `-V` (Unix
    /// convention, e.g. `gcc -V`, `rustc -V`) and `-v` as a short form,
    /// since operators reach for either out of habit and there's no other
    /// use for `-v` on this top-level flag set to conflict with.
    #[arg(short = 'V', long = "version", short_alias = 'v')]
    version: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Print the active configuration as JSON.
    Config,
    /// Compute the desired runner count for given inputs (used by tests/tooling).
    Desired {
        /// Minimum permanent runner pool size.
        min: u32,
        /// Maximum total runner count.
        max: u32,
        /// Currently busy runners.
        busy: u32,
        /// Currently queued self-hosted jobs.
        queued: u32,
    },
    /// Check dependencies and prepare config/state/log directories.
    Setup {
        /// Run the complete first-run wizard in the terminal.
        #[arg(long)]
        terminal: bool,
    },
    /// Add a repository using the currently configured GitHub authentication.
    Connect {
        /// Repository in owner/repository form.
        repository: String,
    },
    /// Quick health check of the current configuration.
    Doctor,
    /// Check for and apply GitRun updates.
    Update {
        /// Update only the permanent runner pool without replacing GitRun itself.
        #[arg(long)]
        only_containers: bool,
        /// Optional manifest URL to check instead of the latest GitHub release.
        manifest_url: Option<String>,
    },
    /// Internal scheduler service entry point.
    #[command(name = "scheduler", hide = true)]
    Scheduler,
    /// Launch the GitRun dashboard (default when no command is given).
    Dashboard,
    /// Internal: run the elevated installation step (invoked by the setup wizard via pkexec).
    #[command(name = "--install-root", hide = true)]
    InstallRoot {
        /// Path to a file containing the GitHub token to install.
        token_path: String,
    },
    /// Roll back to a previous backup created by `update`.
    Rollback {
        /// Path to the backup to restore.
        backup_path: String,
    },
}

fn run_config() -> i32 {
    match load_config() {
        Ok(config) => {
            println!("{}", serde_json::to_string_pretty(&config).unwrap());
            0
        }
        Err(error) => {
            eprintln!("configuration error: {error}");
            2
        }
    }
}

fn run_desired(min: u32, max: u32, busy: u32, queued: u32) -> i32 {
    println!("{}", Runner::desired_count(min, max, busy, queued));
    0
}

fn run_setup() -> i32 {
    match load_config() {
        Ok(config) => {
            let config_dir = setup_config_dir();
            match prepare_directories(&config, config_dir) {
                Ok(report) => {
                    let failed = report.dependencies.iter().filter(|d| !d.available).count();
                    for dependency in &report.dependencies {
                        println!(
                            "{}: {}",
                            dependency.name,
                            if dependency.available {
                                "available"
                            } else {
                                "missing"
                            }
                        );
                    }
                    println!("config: {}", report.config_dir.display());
                    println!("state: {}", report.state_dir.display());
                    println!("logs: {}", report.log_dir.display());
                    if failed != 0 {
                        eprintln!("GitRun setup: FAIL — {failed} dependency check(s) failed");
                        return 1;
                    }
                    println!("GitRun setup: PASS");
                    0
                }
                Err(error) => {
                    eprintln!("GitRun setup: FAIL — {error}");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("GitRun setup: FAIL — {error}");
            1
        }
    }
}

fn run_doctor() -> i32 {
    match load_config() {
        Ok(config) => {
            println!(
                "GitRun doctor: PASS ({} repositories, pool {}..{})",
                config.repositories.len(),
                config.min_runners,
                config.max_runners
            );
            0
        }
        Err(error) => {
            eprintln!("GitRun doctor: FAIL — {error}");
            1
        }
    }
}

fn run_update(manifest_url: Option<&str>, only_containers: bool) -> i32 {
    if only_containers {
        return match gitrun_scheduler::run_gtuu_once() {
            Ok(count) => {
                println!("GitRun GTUU: updated {count} permanent runner(s)");
                0
            }
            Err(error) => {
                eprintln!("GitRun GTUU: FAIL — {error}");
                1
            }
        };
    }
    // update_command's original signature takes a full args slice with the
    // manifest URL at index 1 — preserved as-is rather than refactored, to
    // keep this change scoped to argument *parsing*, not the update logic
    // itself.
    let args: Vec<String> = match manifest_url {
        Some(url) => vec!["update".to_owned(), url.to_owned()],
        None => vec!["update".to_owned()],
    };
    match update_command(&args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun update: FAIL — {error}");
            1
        }
    }
}

fn run_dashboard() -> i32 {
    match dashboard_command() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun dashboard: FAIL — {error}");
            1
        }
    }
}

fn run_install_root(token_path: &str) -> i32 {
    match install_root_command(token_path) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun install: FAIL — {error}");
            1
        }
    }
}

fn run_rollback(backup_path: &str) -> i32 {
    match rollback_command(backup_path) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun rollback: FAIL — {error}");
            1
        }
    }
}
