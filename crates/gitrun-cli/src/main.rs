use gitrun_core::{Config, Runner};
use gitrun_setup::prepare_directories;
use gitrun_updater::{
    apply_update, build_plan, dependency_status, download_and_verify, fetch_manifest, latest_manifest,
    pin_runner_image, refresh_docker_stack, rollback, update_incompatible_dependencies, update_runner_image, BackupRecord, UpdatePaths,
};
use std::path::{Path, PathBuf};

fn help() {
    println!("GitRun");
    println!("Usage: gitrun [version|config|desired|doctor|setup|update|rollback|dashboard|help]");
    println!("  dashboard              open GitRun (first run launches the setup wizard)");
    println!("  setup                  check host dependencies and runtime directories");
    println!("  update                 check and apply the newest GitRun release");
    println!("  rollback <backup.json> restore a previously-backed-up installation");
}

fn parse_u32_arg(args: &[String], index: usize, name: &str) -> Result<u32, String> {
    args.get(index)
        .ok_or_else(|| format!("missing {name}"))
        .and_then(|value| value.parse::<u32>().map_err(|_| format!("invalid {name}: {value}")))
}

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
    std::env::var("GITRUN_VERSION").unwrap_or_else(|_| "0.0.0".into())
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
        let status = dependency_status(name, "0.0.0");
        (name.into(), status.installed_version.or_else(|| {
            if command == "python3" {
                dependency_status(name, "0.0.0").installed_version
            } else {
                None
            }
        }))
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

fn dashboard_command() -> Result<(), Box<dyn std::error::Error>> {
    gitrun_dashboard::run().map_err(|error| error.into())
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("dashboard") {
        "version" if args.len() == 1 => {
            let version = std::env::var("GITRUN_VERSION")
                .ok()
                .or_else(|| std::fs::read_to_string("version.txt").ok().map(|v| v.trim().to_owned()))
                .unwrap_or_else(|| "0.0.0".into());
            println!("GitRun Rust core {version}");
        },
        "config" if args.len() == 1 => match load_config() {
            Ok(config) => println!("{}", serde_json::to_string_pretty(&config).unwrap()),
            Err(error) => { eprintln!("configuration error: {error}"); std::process::exit(2); }
        },
        "desired" if args.len() == 5 => {
            let values = [
                parse_u32_arg(&args, 1, "min"),
                parse_u32_arg(&args, 2, "max"),
                parse_u32_arg(&args, 3, "busy"),
                parse_u32_arg(&args, 4, "queued"),
            ];
            if let Some(error) = values.iter().find_map(|v| v.as_ref().err()) {
                eprintln!("{error}"); std::process::exit(2);
            }
            println!("{}", Runner::desired_count(
                *values[0].as_ref().unwrap(), *values[1].as_ref().unwrap(),
                *values[2].as_ref().unwrap(), *values[3].as_ref().unwrap(),
            ));
        }
        "setup" if args.len() == 1 => match load_config() {
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
                        if failed != 0 { eprintln!("GitRun setup: FAIL — {failed} dependency check(s) failed"); std::process::exit(1); }
                        println!("GitRun setup: PASS");
                    }
                    Err(error) => { eprintln!("GitRun setup: FAIL — {error}"); std::process::exit(1); }
                }
            }
            Err(error) => { eprintln!("GitRun setup: FAIL — {error}"); std::process::exit(1); }
        },
        "doctor" if args.len() == 1 => match load_config() {
            Ok(config) => println!("GitRun doctor: PASS ({} repositories, pool {}..{})", config.repositories.len(), config.min_runners, config.max_runners),
            Err(error) => { eprintln!("GitRun doctor: FAIL — {error}"); std::process::exit(1); }
        },
        "update" if args.len() <= 2 => {
            if let Err(error) = update_command(&args) { eprintln!("GitRun update: FAIL — {error}"); std::process::exit(1); }
        }
        "dashboard" if args.len() == 1 => {
            if let Err(error) = dashboard_command() {
                eprintln!("GitRun dashboard: FAIL — {error}");
                std::process::exit(1);
            }
        }
        "--install-root" if args.len() == 2 => {
            if let Err(error) = install_root_command(&args[1]) {
                eprintln!("GitRun install: FAIL — {error}");
                std::process::exit(1);
            }
        }
        "rollback" if args.len() == 2 => {
            if let Err(error) = rollback_command(&args[1]) { eprintln!("GitRun rollback: FAIL — {error}"); std::process::exit(1); }
        }
        "help" if args.len() == 1 || (args.len() == 2 && args[0] == "--help") => help(),
        _ => { help(); std::process::exit(2); }
    }
}
