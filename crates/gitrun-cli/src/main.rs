use clap::Parser;
use gitrun_core::{AppAuth, Config, GitHubAuth, GitRunApi, Runner};
use gitrun_setup::{bootstrap_linux_with_auth, prepare_directories, BootstrapAuth};
use gitrun_updater::{
    apply_installed_update, apply_update, build_plan, dependency_status, download_and_verify,
    fetch_manifest, latest_manifest, pin_runner_image, rollback, rollback_installed_update,
    update_incompatible_dependencies, update_runner_image, BackupRecord, InstalledArtifact,
    InstalledBackupRecord, UpdatePaths,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod cat;

macro_rules! println {
    () => {
        $crate::cat::presenter::terminal_print(format_args!(""), false, true)
    };
    ($($arg:tt)*) => {
        $crate::cat::presenter::terminal_print(format_args!($($arg)*), false, true)
    };
}

macro_rules! eprintln {
    () => {
        $crate::cat::presenter::terminal_print(format_args!(""), true, true)
    };
    ($($arg:tt)*) => {
        $crate::cat::presenter::terminal_print(format_args!($($arg)*), true, true)
    };
}

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
        .filter(|value| !value.trim().is_empty())
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

fn target_triple() -> Result<String, Box<dyn std::error::Error>> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".into()),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu".into()),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc".into()),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin".into()),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin".into()),
        (os, arch) => Err(format!("unsupported GitRun release target: {os}/{arch}").into()),
    }
}

fn system_install_paths() -> Option<(PathBuf, PathBuf)> {
    let executable = std::env::current_exe().ok()?;
    let is_system_binary = [
        Path::new("/usr/bin/gitrun"),
        Path::new("/usr/local/bin/gitrun"),
    ]
    .iter()
    .any(|path| executable == *path);
    if !is_system_binary {
        return None;
    }

    let version_file = std::env::var("GITRUN_VERSION_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/usr/share/gitrun/version.txt"));
    Some((executable, version_file))
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("id")
            .args(["-u"])
            .output()
            .map(|output| {
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0"
            })
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn elevate_system_update(manifest_url: Option<&str>) -> Result<i32, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new("sudo");
    command.arg(executable).arg("update");
    if let Some(url) = manifest_url {
        command.arg(url);
    }
    // The parent process owns the terminal presenter. Do not let the elevated
    // child create a second cat presenter, otherwise a single command can
    // render the final state twice.
    command.env("GITRUN_NO_CAT", "1");
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
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

fn update_command(
    args: &[String],
    presenter: &mut cat::presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    let manifest = if let Some(url) = args.get(1) {
        fetch_manifest(url)?
    } else {
        latest_manifest(&repository)?
    };
    let current = current_version();
    let target = target_triple()?;
    let plan = build_plan(&manifest, &current, &target, &dependency_snapshot())?;
    presenter.transition(cat::presenter::ValidationState::Loading);

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

    let config_path = persistent_config_path();
    let system_install = system_install_paths();
    let work_root = match system_install {
        Some(_) => PathBuf::from(
            std::env::var("GITRUN_UPDATE_DIR").unwrap_or_else(|_| "/var/lib/gitrun/update".into()),
        ),
        None => PathBuf::from(
            std::env::var("GITRUN_UPDATE_DIR").unwrap_or_else(|_| ".gitrun-update".into()),
        ),
    };
    std::fs::create_dir_all(&work_root)?;
    let archive = work_root.join(&plan.artifact);
    let artifact = manifest.artifact_for(&target)?;
    download_and_verify(&plan.artifact_url, &artifact.sha256, &archive)?;
    presenter.transition(cat::presenter::ValidationState::Working);
    println!("checksum: PASS");

    if let Some((gitrun_binary, version_file)) = system_install {
        let backup_root = PathBuf::from(
            std::env::var("GITRUN_BACKUP_DIR").unwrap_or_else(|_| "/var/lib/gitrun/backups".into()),
        );
        let artifacts = [InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: gitrun_binary.clone(),
            required: true,
        }];
        let backup = apply_installed_update(
            &archive,
            &target,
            &manifest.version,
            &artifacts,
            &version_file,
            &backup_root,
            &gitrun_binary,
            config_path.as_deref(),
        )?;

        if let Some(image) = &plan.runner_image {
            if let Err(error) = update_runner_image(image) {
                let rollback_error = rollback_installed_update(&backup).err();
                return Err(match rollback_error {
                    Some(rollback_error) => format!(
                        "runner update failed: {error}; GitRun rollback also failed: {rollback_error}"
                    ),
                    None => format!("runner update failed; GitRun was rolled back: {error}"),
                }
                .into());
            }
            if let Some(config_file) = config_path.as_deref() {
                if let Err(error) = pin_runner_image(config_file, image) {
                    let rollback_error = rollback_installed_update(&backup).err();
                    return Err(match rollback_error {
                        Some(rollback_error) => format!(
                            "runner configuration update failed: {error}; GitRun rollback also failed: {rollback_error}"
                        ),
                        None => format!(
                            "runner configuration update failed; GitRun was rolled back: {error}"
                        ),
                    }
                    .into());
                }
            }
        }

        presenter.transition(cat::presenter::ValidationState::Running);
        if let Err(error) = restart_scheduler_service() {
            let rollback_result = rollback_installed_update(&backup);
            let restore_result = restart_scheduler_service();
            return match (rollback_result, restore_result) {
                (Ok(()), Ok(())) => Err(format!(
                    "scheduler restart failed; GitRun was rolled back and the previous service was restarted: {error}"
                ).into()),
                (Err(rollback_error), Ok(())) => Err(format!(
                    "scheduler restart failed: {error}; rollback also failed: {rollback_error}"
                ).into()),
                (Ok(()), Err(restore_error)) => Err(format!(
                    "scheduler restart failed and the restored service could not be restarted: {error}; restore restart failed: {restore_error}"
                ).into()),
                (Err(rollback_error), Err(restore_error)) => Err(format!(
                    "scheduler restart failed: {error}; rollback failed: {rollback_error}; restoring the previous service also failed: {restore_error}"
                ).into()),
            };
        }

        presenter.transition(cat::presenter::ValidationState::Success);
        println!("GitRun update: PASS");
        println!(
            "backup: {}",
            backup_root
                .join(format!("system-{}-{}", backup.version, backup.created_at))
                .display()
        );
        return Ok(());
    }

    let install_dir =
        PathBuf::from(std::env::var("GITRUN_INSTALL_DIR").unwrap_or_else(|_| "./gitrun".into()));
    let state_dir =
        PathBuf::from(std::env::var("GITRUN_STATE_DIR").unwrap_or_else(|_| "./state".into()));
    let config_dir = config_path
        .as_ref()
        .and_then(|path| path.parent().map(Path::to_path_buf));
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

    let version_file = paths.install_dir.join("version.txt");
    if let Err(error) = write_version_file(&version_file, &manifest.version) {
        let rollback_error = rollback(&paths, &backup).err();
        return Err(match rollback_error {
            Some(rollback_error) => {
                format!("version metadata update failed: {error}; rollback also failed: {rollback_error}")
            }
            None => format!("version metadata update failed; GitRun was rolled back: {error}"),
        }
        .into());
    }

    if let Some(image) = &plan.runner_image {
        if let Err(error) = update_runner_image(image) {
            rollback(&paths, &backup)?;
            return Err(format!("runner update failed; GitRun was rolled back: {error}").into());
        }
        if let Some(config_file) = config_path.as_deref() {
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
        let rollback_result = rollback(&paths, &backup);
        let restore_result = restart_scheduler_service();

        return match (rollback_result, restore_result) {
            (Ok(()), Ok(())) => {
                Err(format!("scheduler restart failed; GitRun was rolled back and the previous service was restarted: {error}").into())
            }
            (Err(rollback_error), Ok(())) => {
                Err(format!("scheduler restart failed: {error}; rollback also failed: {rollback_error}").into())
            }
            (Ok(()), Err(restore_error)) => {
                Err(format!("scheduler restart failed and the restored service could not be restarted: {error}; restore restart failed: {restore_error}").into())
            }
            (Err(rollback_error), Err(restore_error)) => {
                Err(format!("scheduler restart failed: {error}; rollback failed: {rollback_error}; restoring the previous service also failed: {restore_error}").into())
            }
        };
    }
    presenter.transition(cat::presenter::ValidationState::Success);
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

    if !std::process::Command::new("stty")
        .arg("-echo")
        .status()?
        .success()
    {
        return Err("unable to disable terminal echo for secret input".into());
    }

    let mut value = String::new();
    let input_result = io::stdin().read_line(&mut value);
    let restore_result = std::process::Command::new("stty").arg("echo").status();
    println!();

    if let Err(error) = restore_result {
        return match input_result {
            Ok(_) => {
                Err(format!("unable to restore terminal echo after secret input: {error}").into())
            }
            Err(input_error) => Err(format!(
                "secret input failed: {input_error}; unable to restore terminal echo: {error}"
            )
            .into()),
        };
    }

    if !restore_result
        .expect("restore result already matched")
        .success()
    {
        return match input_result {
            Ok(_) => Err("unable to restore terminal echo after secret input".into()),
            Err(input_error) => Err(format!(
                "secret input failed: {input_error}; unable to restore terminal echo"
            )
            .into()),
        };
    }

    input_result
        .map(|_| value.trim().to_owned())
        .map_err(Into::into)
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

fn write_version_file(path: &Path, version: &str) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".gitrun-version-{}.{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temp)?;
        file.write_all(format!("{version}\n").as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&temp, format!("{version}\n"))?;
    }

    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
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

fn terminal_setup_command(
    presenter: &mut cat::presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
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

    presenter.transition(cat::presenter::ValidationState::Connecting);
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
        presenter.transition(cat::presenter::ValidationState::Working);
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

fn connect_command(
    repository: &str,
    presenter: &mut cat::presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
    if repository.split('/').count() != 2 || repository.split('/').any(|part| part.is_empty()) {
        return Err("usage: gitrun connect <owner/repository>".into());
    }

    let path = persistent_config_path()
        .ok_or("persistent GitRun configuration not found; run 'gitrun setup --terminal' first")?;
    let config = Config::from_env_file(&path)?;
    let auth = GitHubAuth::from_config_file(&config, &path)?;

    verify_repository_access(&auth, repository)?;
    presenter.transition(cat::presenter::ValidationState::Working);

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

    presenter.transition(cat::presenter::ValidationState::Success);
    println!("Repository connected: {repository}");
    println!("Configuration updated: {}", path.display());
    println!("Restart GitRun to apply it to the scheduler.");
    Ok(())
}

fn rollback_command(
    path: &str,
    presenter: &mut cat::presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
    presenter.transition(cat::presenter::ValidationState::Recovering);
    let raw = std::fs::read_to_string(path)?;
    if let Ok(backup) = serde_json::from_str::<InstalledBackupRecord>(&raw) {
        rollback_installed_update(&backup)?;
        presenter.transition(cat::presenter::ValidationState::Success);
        println!("GitRun rollback: PASS");
        return Ok(());
    }

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
    presenter.transition(cat::presenter::ValidationState::Success);
    println!("GitRun rollback: PASS");
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let mut presenter = cat::presenter::CatPresenter::new();

    if cli.gitrun {
        presenter.start_live();
        presenter.transition(cat::presenter::ValidationState::Ready);
        println!("GitRun {}", current_version());
        presenter.finish(0);
        std::process::exit(0);
    }
    if cli.version || cli.all_crates || cli.crate_name.is_some() {
        presenter.start_live();
        presenter.transition(cat::presenter::ValidationState::Ready);
        let exit_code = run_version(cli.crate_name.as_deref());
        presenter.finish(exit_code);
        std::process::exit(exit_code);
    }

    let command = cli.command.unwrap_or(Command::Dashboard);
    let is_cat_command = matches!(&command, Command::Cat);
    let is_status_command = matches!(&command, Command::Status { .. });
    if !is_cat_command && !is_status_command {
        presenter.start_live();
    }
    if !is_cat_command {
        presenter.transition(validation_state_for_command(&command));
    }

    let exit_code = match command {
        Command::Config => run_config(),
        Command::Desired {
            min,
            max,
            busy,
            queued,
        } => run_desired(min, max, busy, queued),
        Command::Status { runner } => run_status(runner, &mut presenter),
        Command::Setup { terminal } => {
            if terminal {
                match terminal_setup_command(&mut presenter) {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("GitRun terminal setup: FAIL — {error}");
                        1
                    }
                }
            } else {
                run_setup(&mut presenter)
            }
        }
        Command::Cat => cat::run(),
        Command::Connect { repository } => match connect_command(&repository, &mut presenter) {
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
        } => run_update(manifest_url.as_deref(), only_containers, &mut presenter),
        Command::Scheduler => run_scheduler(),
        Command::Dashboard => run_dashboard(),
        Command::RecoveryGtuu => run_recovery_gtuu(&mut presenter),
        Command::RepairService => run_repair_service(),
        Command::InstallRoot { token_path } => run_install_root(&token_path),
        Command::Rollback { backup_path } => run_rollback(&backup_path, &mut presenter),
        Command::CheckCompatibility { workflow } => {
            run_check_compatibility(workflow.as_deref(), &mut presenter)
        }
        Command::Settings => run_settings(),
        Command::ApiList => run_api_list(),
        Command::ApiPolicy {
            api,
            operation,
            enabled,
        } => run_api_policy(&api, operation.as_deref(), enabled),
    };

    if !is_cat_command && !is_status_command {
        presenter.finish(exit_code);
    }
    std::process::exit(exit_code);
}

fn validation_state_for_command(command: &Command) -> cat::presenter::ValidationState {
    use cat::presenter::ValidationState;

    match command {
        Command::Config | Command::Settings => ValidationState::Reading,
        Command::Desired { .. } => ValidationState::Working,
        Command::Setup { .. } | Command::InstallRoot { .. } => ValidationState::Setup,
        Command::Cat => ValidationState::Ready,
        Command::Connect { .. } => ValidationState::Connecting,
        Command::Doctor => ValidationState::Validating,
        // Update validates the release plan before entering the updating state.\n        // Keeping the initial state neutral avoids showing "updating" when the\n        // requested release is already installed or otherwise rejected.\n        Command::Update { .. } => ValidationState::Ready,
        Command::Scheduler | Command::Dashboard => ValidationState::Running,
        Command::RecoveryGtuu | Command::RepairService | Command::Rollback { .. } => {
            ValidationState::Recovering
        }
        Command::CheckCompatibility { .. } => ValidationState::Validating,
        Command::ApiList | Command::ApiPolicy { .. } | Command::Status { .. } => {
            ValidationState::Api
        }
    }
}

/// GitRun: self-hosted GitHub Actions runner manager.
#[derive(clap::Parser)]
#[command(name = "gitrun", about, long_about = None, disable_version_flag = true)]
struct Cli {
    /// Print the GitRun version and exit. Accepts both `-V` (Unix
    /// convention, e.g. `gcc -V`, `rustc -V`) and `-v` as a short form,
    /// since operators reach for either out of habit and there's no other
    /// use for `-v` on this top-level flag set to conflict with.
    #[arg(short = 'V', long = "version", short_alias = 'v', alias = "v")]
    version: bool,
    /// Print only the GitRun program version, without workspace crate versions.
    #[arg(long = "gitrun")]
    gitrun: bool,
    /// Show one crate version; omit it to show GitRun and every workspace crate.
    #[arg(long = "crate")]
    crate_name: Option<String>,
    /// Show GitRun and every GitRun crate version.
    #[arg(long = "crates")]
    all_crates: bool,
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
    /// Inspect a GitHub Actions runner and its GitRun container.
    Status {
        /// GitHub Actions runner ID.
        #[arg(long)]
        runner: u64,
    },
    /// Check dependencies and prepare config/state/log directories.
    Setup {
        /// Run the complete first-run wizard in the terminal.
        #[arg(long)]
        terminal: bool,
    },
    /// Play the GitRun terminal cat easter egg.
    Cat,
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
    /// Internal recovery command used by the protected recovery UI.
    #[command(name = "recovery-gtuu", hide = true)]
    RecoveryGtuu,
    /// Internal recovery command used for privileged systemd repair.
    #[command(name = "repair-service", hide = true)]
    RepairService,
    /// Internal: run the elevated installation step (invoked by the setup wizard).
    #[command(name = "--install-root", hide = true)]
    InstallRoot {
        /// Path to the credential-bearing setup request file.
        token_path: String,
    },
    /// Roll back to a previous backup created by `update`.
    Rollback {
        /// Path to the backup to restore.
        backup_path: String,
    },
    /// Analyze configured GitHub Actions workflow files for compatibility and GitDockRun requirements.
    CheckCompatibility {
        /// Workflow file or directory to analyze. Defaults to .github/workflows.
        workflow: Option<String>,
    },
    /// Print the current GitRun security/API settings as JSON.
    Settings,
    /// List the closed GitRun API surface.
    #[command(name = "api-list")]
    ApiList,
    /// Inspect or describe a GitRun API policy entry.
    #[command(name = "api-policy")]
    ApiPolicy {
        /// GitRun API name, e.g. GitDockRun.
        api: String,
        /// Optional API operation to inspect.
        #[arg(long)]
        operation: Option<String>,
        /// Optional requested global enablement state.
        #[arg(long)]
        enabled: Option<bool>,
    },
}

fn run_version(crate_name: Option<&str>) -> i32 {
    let version = current_version();
    let lock = include_str!("../../../Cargo.lock");
    let mut crates = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let mut name = None;
        let mut ver = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("name = \")") {
                name = value.strip_suffix('"');
            }
            if let Some(value) = line.strip_prefix("version = \")") {
                ver = value.strip_suffix('"');
            }
            if name.is_some() && ver.is_some() {
                break;
            }
        }
        if let (Some(name), Some(ver)) = (name, ver) {
            if name.starts_with("gitrun-")
                && !crates.iter().any(|(n, _): &(String, String)| n == name)
            {
                crates.push((name.to_owned(), ver.to_owned()));
            }
        }
    }
    crates.sort();
    if let Some(name) = crate_name {
        let wanted = if name == "gitrun" {
            "gitrun".into()
        } else if name.starts_with("gitrun-") {
            name.to_owned()
        } else {
            format!("gitrun-{name}")
        };
        if wanted == "gitrun" {
            println!("GitRun {version}");
        } else if let Some((_, ver)) = crates.iter().find(|(n, _)| n == &wanted) {
            println!("{wanted} {ver}");
        } else {
            eprintln!("unknown GitRun crate: {name}");
            return 2;
        }
    } else {
        println!("GitRun {version}");
        for (name, ver) in crates {
            println!("{name} {ver}");
        }
    }
    0
}

fn build_status_github_client(
    config: &Config,
) -> Result<gitrun_scheduler::GitHubClient, Box<dyn std::error::Error>> {
    let connect_timeout = std::time::Duration::from_secs(config.github_connect_timeout);
    let request_timeout = std::time::Duration::from_secs(config.github_request_timeout);

    match GitHubAuth::from_config(config)? {
        GitHubAuth::App(auth) => Ok(gitrun_scheduler::GitHubClient::with_app_auth(
            auth,
            connect_timeout,
            request_timeout,
        )?),
        GitHubAuth::Pat(token) => Ok(gitrun_scheduler::GitHubClient::with_timeouts(
            token,
            connect_timeout,
            request_timeout,
        )?),
    }
}

fn runner_validation_state(
    runner: &gitrun_scheduler::Runner,
    container: Option<&gitrun_scheduler::docker::ManagedContainer>,
) -> cat::presenter::ValidationState {
    use cat::presenter::ValidationState;

    if !runner.is_online() {
        return ValidationState::Failure;
    }
    if let Some(container) = container {
        if !container.status.eq_ignore_ascii_case("running") {
            return ValidationState::Recovering;
        }
    }
    if runner.busy {
        ValidationState::Running
    } else {
        ValidationState::Ready
    }
}

fn runner_cat_state(
    runner: &gitrun_scheduler::Runner,
    container: Option<&gitrun_scheduler::docker::ManagedContainer>,
    commands: &[String],
) -> &'static str {
    use cat::presenter::ValidationState;

    if !runner.is_online() {
        return "sad";
    }
    let Some(container) = container else {
        return if runner.busy { "runner" } else { "seated" };
    };
    if !container.status.eq_ignore_ascii_case("running") {
        return "recovery";
    }
    if !runner.busy {
        return "seated";
    }

    let joined = commands.join(" ").to_ascii_lowercase();

    if joined.contains("cargo test")
        || joined.contains("cargo nextest")
        || joined.contains("pytest")
        || joined.contains("npm test")
        || joined.contains("pnpm test")
        || joined.contains("yarn test")
    {
        return "testing";
    }
    if joined.contains("cargo build")
        || joined.contains("cargo check")
        || joined.contains("cargo clippy")
        || joined.contains("cargo rustc")
    {
        return "compiling";
    }
    if joined.contains("rustc") || joined.contains("cargo ") {
        return "rust";
    }
    if joined.contains("docker build")
        || joined.contains("docker compose")
        || joined.contains("docker pull")
        || joined.contains("docker push")
    {
        return "docker";
    }
    if joined.contains("npm run build")
        || joined.contains("npm run tauri")
        || joined.contains("pnpm build")
        || joined.contains("yarn build")
    {
        return "building";
    }
    if joined.contains("git ") {
        return "working";
    }
    if joined.contains("setup") {
        return "setup";
    }
    if joined.contains("curl ") || joined.contains("wget ") {
        return "loading";
    }

    let _ = ValidationState::Running;
    "runner"
}

fn run_status(runner_id: u64, presenter: &mut cat::presenter::CatPresenter) -> i32 {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            presenter.show_named("failed");
            eprintln!("configuration error: {error}");
            return 2;
        }
    };

    presenter.transition(cat::presenter::ValidationState::Api);

    let client = match build_status_github_client(&config) {
        Ok(client) => client,
        Err(error) => {
            presenter.show_named("failed");
            eprintln!("GitRun status: unable to initialize GitHub client: {error}");
            return 2;
        }
    };

    let mut matches = Vec::new();
    for repository in &config.repositories {
        match client.list_runners(repository) {
            Ok(runners) => {
                if let Some(runner) = runners.into_iter().find(|runner| runner.id == runner_id) {
                    matches.push((repository.clone(), runner));
                }
            }
            Err(error) => {
                presenter.show_named("failed");
                eprintln!("GitRun status: unable to query {repository}: {error}");
                return 1;
            }
        }
    }

    let Some((repository, runner)) = matches.into_iter().next() else {
        presenter.show_named("unknown");
        eprintln!("GitRun status: runner {runner_id} was not found in configured repositories");
        return 1;
    };

    let containers = match gitrun_scheduler::docker::managed_containers(&repository) {
        Ok(containers) => containers,
        Err(error) => {
            presenter.show_named("failed");
            eprintln!("GitRun status: unable to inspect GitRun runner containers: {error}");
            return 1;
        }
    };

    let container = containers
        .iter()
        .find(|container| container.name == runner.name);
    let commands = if container
        .map(|container| container.status.eq_ignore_ascii_case("running"))
        .unwrap_or(false)
    {
        match gitrun_scheduler::docker::container_command_lines_on(
            &gitrun_scheduler::docker::DockerHost::Local,
            &runner.name,
        ) {
            Ok(commands) => commands,
            Err(error) => {
                presenter.show_named("failed");
                eprintln!("GitRun status: unable to inspect runner activity: {error}");
                return 1;
            }
        }
    } else {
        Vec::new()
    };

    let validation_state = runner_validation_state(&runner, container);
    let cat_state = runner_cat_state(&runner, container, &commands);
    presenter.show_named(cat_state);

    println!("runner_id: {}", runner.id);
    println!("runner_name: {}", runner.name);
    println!("repository: {}", repository);
    println!("status: {}", runner.status);
    println!("busy: {}", runner.busy);
    println!(
        "container: {}",
        container
            .map(|container| container.status.as_str())
            .unwrap_or("not-found")
    );
    println!("Validation State: {:?}", validation_state);
    println!("Cat State: {}", cat_state);

    0
}

fn run_api_list() -> i32 {
    for api in GitRunApi::ALL {
        println!("{}", api.as_str());
    }
    0
}

fn run_settings() -> i32 {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return 2;
        }
    };
    let settings = match gitrun_core::GitRunSettings::load_or_default(
        gitrun_core::GitRunSettings::path_for_state_dir(&config.state_dir),
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("settings error: {error}");
            return 2;
        }
    };
    match serde_json::to_string_pretty(&settings) {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("settings serialization error: {error}");
            2
        }
    }
}

fn run_api_policy(api_name: &str, operation: Option<&str>, enabled: Option<bool>) -> i32 {
    let api = GitRunApi::ALL
        .into_iter()
        .find(|api| api.as_str().eq_ignore_ascii_case(api_name));
    let Some(api) = api else {
        eprintln!("unknown GitRun API: {api_name}");
        return 2;
    };
    if let Some(operation) = operation {
        let op = [
            gitrun_core::GitRunOperation::Read,
            gitrun_core::GitRunOperation::Write,
            gitrun_core::GitRunOperation::Exists,
            gitrun_core::GitRunOperation::Delete,
            gitrun_core::GitRunOperation::List,
            gitrun_core::GitRunOperation::Connect,
            gitrun_core::GitRunOperation::Disconnect,
            gitrun_core::GitRunOperation::Execute,
            gitrun_core::GitRunOperation::Melt,
            gitrun_core::GitRunOperation::File,
            gitrun_core::GitRunOperation::Logs,
            gitrun_core::GitRunOperation::Register,
            gitrun_core::GitRunOperation::Install,
            gitrun_core::GitRunOperation::Remove,
            gitrun_core::GitRunOperation::Update,
            gitrun_core::GitRunOperation::Verify,
            gitrun_core::GitRunOperation::Status,
        ]
        .into_iter()
        .find(|op| op.as_str().eq_ignore_ascii_case(operation));
        let Some(op) = op else {
            eprintln!("unknown GitRun operation: {operation}");
            return 2;
        };
        if !api.supports_operation(op) {
            eprintln!("{operation} is not part of {}", api.as_str());
            return 2;
        }
        println!("{} {} supported", api.as_str(), op.as_str());
        return 0;
    }
    println!("{}", api.as_str());
    if let Some(enabled) = enabled {
        println!("requested global enablement: {enabled}");
    }
    0
}

fn run_check_compatibility(
    workflow: Option<&str>,
    presenter: &mut cat::presenter::CatPresenter,
) -> i32 {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return 2;
        }
    };
    let settings_path = gitrun_core::GitRunSettings::path_for_state_dir(&config.state_dir);
    let settings = match gitrun_core::GitRunSettings::load_or_default(&settings_path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("settings error: {error}");
            return 2;
        }
    };
    let repo = match config.repositories.first() {
        Some(repo) => repo,
        None => {
            eprintln!("no repository configured");
            return 2;
        }
    };
    let effective = settings.effective_for_repository(repo);
    let root = workflow
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".github/workflows"));
    let files = if root.is_file() {
        match std::fs::read_to_string(&root) {
            Ok(content) => vec![(root.display().to_string(), content)],
            Err(error) => {
                eprintln!("cannot read workflow: {error}");
                return 2;
            }
        }
    } else {
        match std::fs::read_dir(&root) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let p = entry.path();
                    if !matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("yml") | Some("yaml")
                    ) {
                        return None;
                    }
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|content| (p.display().to_string(), content))
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                eprintln!("cannot read workflow directory: {error}");
                return 2;
            }
        }
    };
    let mut exit_code = 0;
    for (name, content) in files {
        presenter.transition(cat::presenter::ValidationState::Testing);
        let report = gitrun_core::analyze_compatibility(&name, &content, &effective);
        println!("{}: {:?}", report.workflow, report.status);
        for finding in report.findings {
            println!(
                "  [{:?}] {} — {}",
                finding.status, finding.code, finding.message
            );
            if let Some(recommendation) = finding.recommendation {
                println!("    → {recommendation}");
            }
        }
        if report.status == gitrun_core::CompatibilityStatus::Incompatible {
            exit_code = 1;
        }
    }
    exit_code
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

fn run_setup(presenter: &mut cat::presenter::CatPresenter) -> i32 {
    match load_config() {
        Ok(config) => {
            let config_dir = setup_config_dir();
            match prepare_directories(&config, config_dir) {
                Ok(report) => {
                    presenter.transition(cat::presenter::ValidationState::Working);
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
                    presenter.transition(cat::presenter::ValidationState::Success);
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

fn run_update(
    manifest_url: Option<&str>,
    only_containers: bool,
    presenter: &mut cat::presenter::CatPresenter,
) -> i32 {
    if only_containers {
        presenter.transition(cat::presenter::ValidationState::Running);
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

    if system_install_paths().is_some() && !running_as_root() {
        match elevate_system_update(manifest_url) {
            Ok(code) => return code,
            Err(error) => {
                eprintln!("GitRun update: unable to elevate system update: {error}");
                return 1;
            }
        }
    }

    // update_command's original signature takes a full args slice with the
    // manifest URL at index 1 — preserved as-is rather than refactored, to
    // keep this change scoped to argument *parsing*, not the update logic
    // itself.
    let args: Vec<String> = match manifest_url {
        Some(url) => vec!["update".to_owned(), url.to_owned()],
        None => vec!["update".to_owned()],
    };
    match update_command(&args, presenter) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun update: FAIL — {error}");
            1
        }
    }
}

fn reexec_self(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(executable).args(args).exec();
        Err(error.into())
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(executable).args(args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn report_critical_startup(report: &gitrun_recovery::RecoveryReport) {
    eprintln!("GitRun Recovery: startup blocked by a critical issue.");
    for issue in &report.issues {
        if issue.severity == gitrun_recovery::Severity::Critical {
            eprintln!("  {}: {}", issue.title, issue.detail);
        }
    }
}

fn run_dashboard() -> i32 {
    match gitrun_recovery::startup(gitrun_recovery::StartupTarget::Dashboard) {
        Ok(report) if report.update.applied => match reexec_self(&["dashboard"]) {
            Ok(()) => 1,
            Err(error) => {
                eprintln!("GitRun dashboard: unable to restart after update: {error}");
                1
            }
        },
        Ok(report) if report.has_critical() => {
            report_critical_startup(&report);
            if std::env::var_os("DISPLAY").is_some()
                || std::env::var_os("WAYLAND_DISPLAY").is_some()
            {
                gitrun_recovery::ui::run();
                0
            } else {
                eprintln!("GitRun Recovery UI is unavailable without a graphical session.");
                1
            }
        }
        Ok(report) => {
            // A pristine installation has no persistent state directory yet.
            // Do not attempt to record startup health before the first-run
            // setup has created the configured state path.
            if report.config_ok {
                if let Err(error) = gitrun_recovery::mark_startup_healthy() {
                    eprintln!("GitRun: unable to persist startup health: {error}");
                }
            }
            gitrun_dashboard_tauri_lib::run();
            0
        }
        Err(error) => {
            eprintln!("GitRun dashboard: recovery preflight failed: {error}");
            1
        }
    }
}

fn run_scheduler() -> i32 {
    match gitrun_recovery::startup(gitrun_recovery::StartupTarget::Scheduler) {
        Ok(report) if report.update.applied => match reexec_self(&["scheduler"]) {
            Ok(()) => 1,
            Err(error) => {
                eprintln!("GitRun scheduler: unable to restart after update: {error}");
                1
            }
        },
        Ok(report) if report.has_critical() => {
            report_critical_startup(&report);
            1
        }
        Ok(_) => {
            if let Err(error) = gitrun_recovery::mark_startup_healthy() {
                eprintln!("GitRun: unable to persist startup health: {error}");
            }
            gitrun_scheduler::run();
            0
        }
        Err(error) => {
            eprintln!("GitRun scheduler: recovery preflight failed: {error}");
            1
        }
    }
}

fn run_recovery_gtuu(presenter: &mut cat::presenter::CatPresenter) -> i32 {
    presenter.transition(cat::presenter::ValidationState::Recovering);
    match gitrun_recovery::run_gtuu() {
        Ok(report) => {
            if let Some(error) = report.gitrun_update_error {
                eprintln!("GitRun GTUU: FAIL — {error}");
                1
            } else if let Some(error) = report.runner_image_error {
                eprintln!("GitRun GTUU runner image: FAIL — {error}");
                1
            } else if let Some(error) = report.containers_error {
                eprintln!("GitRun GTUU containers: FAIL — {error}");
                1
            } else {
                0
            }
        }
        Err(error) => {
            eprintln!("GitRun GTUU: FAIL — {error}");
            1
        }
    }
}

fn run_repair_service() -> i32 {
    match gitrun_recovery::repair_service_unit() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun service repair: FAIL — {error}");
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

fn run_rollback(backup_path: &str, presenter: &mut cat::presenter::CatPresenter) -> i32 {
    match rollback_command(backup_path, presenter) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun rollback: FAIL — {error}");
            1
        }
    }
}
