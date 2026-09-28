use clap::Parser;
use gitrun_core::{Config, Runner};
use gitrun_setup::prepare_directories;
use gitrun_updater::{
    apply_update, build_plan, dependency_status, download_and_verify, fetch_manifest, latest_manifest,
    pin_runner_image, refresh_docker_stack, rollback, update_incompatible_dependencies, update_runner_image, BackupRecord, UpdatePaths,
};
use std::path::{Path, PathBuf};

fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        return Config::from_env_file(path);
    }
    Config::from_env()
}

fn setup_config_dir() -> PathBuf {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_DIR") {
        return path.into();
    }
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        if let Some(parent) = Path::new(&path).parent() {
            return parent.to_path_buf();
        }
    }
    "config".into()
}

fn current_version() -> String {
    std::env::var("GITRUN_VERSION")
        .ok()
        .or_else(|| std::fs::read_to_string("/usr/share/gitrun/version.txt").ok().map(|v| v.trim().to_owned()))
        .or_else(|| std::fs::read_to_string("version.txt").ok().map(|v| v.trim().to_owned()))
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

    println!("GitRun update: {} -> {}", plan.current_version, plan.target_version);
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
        std::env::var("GITRUN_UPDATE_DIR")
            .unwrap_or_else(|_| ".gitrun-update".into()),
    );
    std::fs::create_dir_all(&work_root)?;
    let archive = work_root.join(&plan.artifact);
    let artifact = manifest.artifact_for(&target)?;
    download_and_verify(&plan.artifact_url, &artifact.sha256, &archive)?;
    println!("checksum: PASS");

    let install_dir = PathBuf::from(
        std::env::var("GITRUN_INSTALL_DIR").unwrap_or_else(|_| "./gitrun".into()),
    );
    let state_dir = PathBuf::from(
        std::env::var("GITRUN_STATE_DIR").unwrap_or_else(|_| "./state".into()),
    );
    let config_dir = std::env::var("GITRUN_CONFIG_DIR").ok().map(PathBuf::from);
    let backup_root = PathBuf::from(
        std::env::var("GITRUN_BACKUP_DIR").unwrap_or_else(|_| "./backups".into()),
    );
    let service_config = std::env::var("GITRUN_SERVICE_CONFIG").ok().map(PathBuf::from);

    let paths = UpdatePaths { install_dir, state_dir, config_dir, service_config, backup_root };
    let backup = apply_update(&paths, &archive, &target, &manifest.version, true)?;
    if let Some(image) = &plan.runner_image {
        if let Err(error) = update_runner_image(image) {
            rollback(&paths, &backup)?;
            return Err(format!("runner update failed; GitRun was rolled back: {error}").into());
        }
        if let Ok(config_file) = std::env::var("GITRUN_CONFIG_FILE") {
            if let Err(error) = pin_runner_image(config_file, image) {
                rollback(&paths, &backup)?;
                return Err(format!("runner configuration update failed; GitRun was rolled back: {error}").into());
            }
        }
    }
    if let Ok(compose) = std::env::var("GITRUN_COMPOSE_FILE") {
        if let Err(error) = refresh_docker_stack(&compose) {
            rollback(&paths, &backup)?;
            return Err(format!("Docker refresh failed; GitRun was rolled back: {error}").into());
        }
    }

    let version_file = paths.install_dir.join("version.txt");
    std::fs::write(version_file, format!("{}\n", manifest.version))?;
    println!("GitRun update: PASS");
    println!("backup: {}", backup.install_backup.parent().unwrap_or(Path::new(".")).display());
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace("'", "'\\''"))
}

fn dashboard_command() -> Result<(), Box<dyn std::error::Error>> {
    match gitrun_dashboard::run() {
        Ok(()) => Ok(()),
        Err(error) => {
            let error_message = error.to_string();
            let executable = std::env::current_exe().map_err(|current_error| {
                format!("{error_message}; unable to locate GitRun CLI: {current_error}")
            })?;
            let executable = executable.to_string_lossy();

            let emergency_command = format!(
                "printf '%s\\n\\n' 'GitRun dashboard failed:'; printf '%s\\n\\n' {}; printf '%s\\n' 'GitRun CLI:'; {} version; printf '%s\\n' 'GitRun doctor:'; {} doctor; printf '%s\\n' 'Press Enter to close.'; read -r",
                shell_quote(&error_message),
                shell_quote(&executable),
                shell_quote(&executable),
            );

            let candidates: [(&str, &[&str]); 3] = [
                ("x-terminal-emulator", &["-e", "sh", "-c", &emergency_command]),
                ("gnome-terminal", &["--", "sh", "-c", &emergency_command]),
                ("konsole", &["-e", "sh", "-c", &emergency_command]),
            ];

            for (program, args) in candidates {
                if std::process::Command::new(program).args(args).spawn().is_ok() {
                    return Err(format!(
                        "dashboard failed; emergency GitRun CLI opened in {program}"
                    )
                    .into());
                }
            }

            Err(format!(
                "dashboard failed: {error_message}; no supported terminal emulator was available"
            )
            .into())
        }
    }
}


fn install_root_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let mut lines = raw.lines();
    let token = lines.next().unwrap_or_default().trim();
    let repositories = lines.next().unwrap_or_default().trim();
    let owner_uid = std::env::var("PKEXEC_UID").ok().and_then(|value| value.parse::<u32>().ok());
    let executable = std::env::current_exe()?;

    gitrun_setup::bootstrap_linux(token, repositories, &executable, owner_uid)?;
    println!("GitRun setup: PASS");
    Ok(())
}

fn rollback_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let backup: BackupRecord = serde_json::from_str(&raw)?;
    let install_dir = PathBuf::from(std::env::var("GITRUN_INSTALL_DIR").unwrap_or_else(|_| "./gitrun".into()));
    let state_dir = PathBuf::from(std::env::var("GITRUN_STATE_DIR").unwrap_or_else(|_| "./state".into()));
    let config_dir = std::env::var("GITRUN_CONFIG_DIR").ok().map(PathBuf::from);
    let backup_root = backup.install_backup.parent().and_then(Path::parent).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./backups"));
    let service_config = std::env::var("GITRUN_SERVICE_CONFIG").ok().map(PathBuf::from);
    let paths = UpdatePaths { install_dir, state_dir, config_dir, service_config, backup_root };
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
        Command::Desired { min, max, busy, queued } => run_desired(min, max, busy, queued),
        Command::Setup => run_setup(),
        Command::Doctor => run_doctor(),
        Command::Update { manifest_url } => run_update(manifest_url.as_deref()),
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
    Setup,
    /// Quick health check of the current configuration.
    Doctor,
    /// Check for and apply GitRun updates.
    Update {
        /// Optional manifest URL to check instead of the latest GitHub release.
        manifest_url: Option<String>,
    },
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
        Ok(config) => { println!("{}", serde_json::to_string_pretty(&config).unwrap()); 0 }
        Err(error) => { eprintln!("configuration error: {error}"); 2 }
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
                        println!("{}: {}", dependency.name, if dependency.available { "available" } else { "missing" });
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
                Err(error) => { eprintln!("GitRun setup: FAIL — {error}"); 1 }
            }
        }
        Err(error) => { eprintln!("GitRun setup: FAIL — {error}"); 1 }
    }
}

fn run_doctor() -> i32 {
    match load_config() {
        Ok(config) => {
            println!("GitRun doctor: PASS ({} repositories, pool {}..{})", config.repositories.len(), config.min_runners, config.max_runners);
            0
        }
        Err(error) => { eprintln!("GitRun doctor: FAIL — {error}"); 1 }
    }
}

fn run_update(manifest_url: Option<&str>) -> i32 {
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
        Err(error) => { eprintln!("GitRun update: FAIL — {error}"); 1 }
    }
}

fn run_dashboard() -> i32 {
    match dashboard_command() {
        Ok(()) => 0,
        Err(error) => { eprintln!("GitRun dashboard: FAIL — {error}"); 1 }
    }
}

fn run_install_root(token_path: &str) -> i32 {
    match install_root_command(token_path) {
        Ok(()) => 0,
        Err(error) => { eprintln!("GitRun install: FAIL — {error}"); 1 }
    }
}

fn run_rollback(backup_path: &str) -> i32 {
    match rollback_command(backup_path) {
        Ok(()) => 0,
        Err(error) => { eprintln!("GitRun rollback: FAIL — {error}"); 1 }
    }
}
