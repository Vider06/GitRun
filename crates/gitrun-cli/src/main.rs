use clap::Parser;
use gitrun_core::{AppAuth, Config, GitHubAuth, GitRunApi, GitRunSettings, Runner};
use gitrun_setup::{bootstrap_linux_with_auth_and_profile, prepare_directories, BootstrapAuth};
use gitrun_smart_scheduler::KittySettings;
use gitrun_updater::{
    apply_installed_update, apply_update, build_plan, dependency_status, download_and_verify,
    fetch_manifest, latest_manifest, pin_runner_image, release_manifest_download_url, rollback,
    rollback_installed_update_with_config, update_incompatible_dependencies, update_runner_image,
    BackupRecord, InstalledArtifact, InstalledBackupRecord, UpdateError, UpdatePaths,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod cat;
mod manage;
mod presenter;

include!(concat!(env!("OUT_DIR"), "/gitrun_crate_versions.rs"));

macro_rules! println {
    () => {
        $crate::presenter::terminal_print(format_args!(""), false, true)
    };
    ($($arg:tt)*) => {
        $crate::presenter::terminal_print(format_args!($($arg)*), false, true)
    };
}

macro_rules! eprintln {
    () => {
        $crate::presenter::terminal_print(format_args!(""), true, true)
    };
    ($($arg:tt)*) => {
        $crate::presenter::terminal_print(format_args!($($arg)*), true, true)
    };
}

const TRUSTED_SYSTEM_CONFIG_FILE: &str = "/etc/gitrun/gitrun.env";

fn select_update_config_path(
    system_install: bool,
    configured_path: Option<PathBuf>,
) -> Option<PathBuf> {
    if system_install {
        // Installed system updates must never write to an environment-selected
        // path. systemd's canonical EnvironmentFile is /etc/gitrun/gitrun.env.
        Some(PathBuf::from(TRUSTED_SYSTEM_CONFIG_FILE))
    } else {
        configured_path
    }
}

fn trusted_system_config_path() -> Option<PathBuf> {
    let path = PathBuf::from(TRUSTED_SYSTEM_CONFIG_FILE);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // System config can contain credentials. Refuse unsafe ownership or
        // group/world write access instead of silently reading/updating it.
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return None;
        }
    }
    Some(path)
}

fn update_config_path(system_install: bool) -> Option<PathBuf> {
    let configured_path = if system_install {
        // Validate the fixed path itself before selection. Do not permit a
        // missing or unsafe trusted file to fall back to an environment path.
        trusted_system_config_path()?
    } else {
        persistent_config_path()?
    };
    select_update_config_path(system_install, Some(configured_path))
}

fn initialize_system_update_config_path(system_install: bool) {
    if system_install {
        // load_config() honors these variables for developer installs. Pin the
        // installed update process to the service's trusted config and discard
        // an alternate directory that could otherwise redirect root-side work.
        std::env::set_var("GITRUN_CONFIG_FILE", TRUSTED_SYSTEM_CONFIG_FILE);
        std::env::remove_var("GITRUN_CONFIG_DIR");
    }
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
    // In a system installation, the root-owned version file is authoritative.
    // Never let an inherited GITRUN_VERSION value influence a privileged update
    // decision; that could suppress an update or misreport the installed version.
    if system_install_paths().is_some() {
        return std::fs::read_to_string("/usr/share/gitrun/version.txt")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "0.0.0".into());
    }

    std::env::var("GITRUN_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/usr/share/gitrun/version.txt")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string("version.txt")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
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

    // Never let inherited environment variables redirect a privileged update.
    let version_file = PathBuf::from("/usr/share/gitrun/version.txt");
    Some((executable, version_file))
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("/usr/bin/id")
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

const DEFAULT_UPDATE_PUBLIC_KEY_HEX: &str =
    "11cc9eaf0147dada407ffd3497c2cebce17d195e21c06030c1e2e1dc688f41f1"; // DevSkim: ignore DS173237 because this is a public Ed25519 verification key, not a secret.
const DEFAULT_UPDATE_PUBLIC_KEY_ID: &str = "gitrun-release-ed25519-v1";

fn update_signing_environment() -> Vec<(String, String)> {
    vec![
        (
            "GITRUN_UPDATE_PUBLIC_KEY_HEX".to_owned(),
            DEFAULT_UPDATE_PUBLIC_KEY_HEX.to_owned(),
        ),
        (
            "GITRUN_UPDATE_PUBLIC_KEY_ID".to_owned(),
            DEFAULT_UPDATE_PUBLIC_KEY_ID.to_owned(),
        ),
        (
            "GITRUN_UPDATE_SIGNATURE_REQUIRED".to_owned(),
            "true".to_owned(),
        ),
    ]
}

fn update_signing_environment_changes(
    force_trusted_values: bool,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    update_signing_environment()
        .into_iter()
        .filter(|(key, _)| force_trusted_values || !is_set(key))
        .collect()
}

fn initialize_update_signing_environment(force_trusted_values: bool) {
    for (key, value) in update_signing_environment_changes(force_trusted_values, |key| {
        std::env::var_os(key).is_some()
    }) {
        std::env::set_var(key, value);
    }
}

fn elevate_system_update(
    manifest_url: Option<&str>,
    force: bool,
    presenter: &mut presenter::CatPresenter,
) -> Result<i32, Box<dyn std::error::Error>> {
    // sudo writes its password prompt directly to the inherited TTY. End the
    // parent's Ratatui inline viewport first so its render thread cannot race
    // with raw child output and corrupt the cat/prompt layout.
    presenter.prepare_for_external_process();

    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new("/usr/bin/sudo");
    for (key, value) in update_signing_environment() {
        command.arg("env").arg(format!("{key}={value}"));
    }
    command.arg(executable).arg("--no-cat").arg("update");
    if force {
        command.arg("--force");
    }
    if let Some(url) = manifest_url {
        command.arg(url);
    }
    // Use an explicit CLI flag instead of an environment variable because
    // sudo may filter custom environment variables.
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(target_os = "linux")]
fn run_embedded_privileged_helper(
    prefix: &str,
    script: &str,
    presenter: &mut presenter::CatPresenter,
) -> i32 {
    if system_install_paths().is_none() {
        eprintln!("GitRun: this operation requires the trusted system installation at /usr/bin/gitrun or /usr/local/bin/gitrun.");
        return 2;
    }

    use std::os::unix::fs::OpenOptionsExt;
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("GitRun: unable to resolve the installed executable: {error}");
            return 1;
        }
    };
    let nonce = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(value) => value.as_nanos(),
        Err(error) => {
            eprintln!("GitRun: unable to create a private helper name: {error}");
            return 1;
        }
    };
    let script_path =
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}.sh", std::process::id()));

    let result = (|| -> Result<i32, Box<dyn std::error::Error>> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&script_path)?;
        file.write_all(script.as_bytes())?;
        file.sync_all()?;
        presenter.prepare_for_external_process();
        let status = std::process::Command::new("/bin/bash")
            .arg(&script_path)
            .arg(&executable)
            .status()?;
        Ok(status.code().unwrap_or(1))
    })();

    let _ = std::fs::remove_file(&script_path);
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("GitRun {prefix}: failed to launch privileged helper: {error}");
            1
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_embedded_privileged_helper(
    _prefix: &str,
    _script: &str,
    _presenter: &mut presenter::CatPresenter,
) -> i32 {
    eprintln!("GitRun uninstall/reinstall currently supports Linux x86_64 only.");
    2
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

fn update_check_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    let manifest_url = args.get(2).filter(|value| !value.starts_with("--"));
    let manifest = match manifest_url {
        Some(url) => fetch_manifest(url)?,
        None => latest_manifest(&repository)?,
    };

    let current = current_version();
    let target = target_triple()?;
    println!("GitRun update check");
    println!("Current version: {current}");
    println!("Latest signed release manifest: {}", manifest.version);

    match build_plan(&manifest, &current, &target, &dependency_snapshot()) {
        Ok(plan) => {
            println!(
                "Update available: {} -> {}",
                plan.current_version, plan.target_version
            );
            println!("Target: {}", plan.target);
            println!("Artifact: {}", plan.artifact);
            for dependency in &plan.dependencies {
                println!(
                    "{}: {} ({})",
                    dependency.name,
                    dependency.installed_version.as_deref().unwrap_or("missing"),
                    dependency.action
                );
            }
        }
        Err(UpdateError::NotNewer) => {
            println!("No update available; GitRun is up to date ({current}).");
        }
        Err(error) => return Err(error.into()),
    }

    if let Some(image) = &manifest.runner_image {
        if !image.reference.contains('/') {
            println!(
                "Runner image target: local {} (the update will check its complete build fingerprint).",
                image.reference
            );
        } else {
            println!("Runner image target: {}@{}", image.reference, image.digest);
            let inspection = std::process::Command::new("docker")
                .args([
                    "image",
                    "inspect",
                    "--format",
                    "{{json .RepoDigests}}",
                    &image.reference,
                ])
                .output();
            match inspection {
                Ok(output) if output.status.success() => {
                    let digests = String::from_utf8_lossy(&output.stdout);
                    if digests.contains(&image.digest) {
                        println!("Runner image is up to date (digest verified).");
                    } else {
                        println!("Runner image update available: configured image does not match the release digest.");
                    }
                }
                Ok(output) => {
                    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    println!(
                        "Runner image status unknown: {}",
                        if detail.is_empty() {
                            "Docker image inspect failed"
                        } else {
                            detail.as_str()
                        }
                    );
                }
                Err(error) => {
                    println!("Runner image status unknown: Docker is unavailable ({error}).")
                }
            }
        }
    } else {
        println!("Runner image: this release manifest does not declare a runner image.");
    }

    println!(
        "Read-only check complete; no files, dependencies, runner images or services were changed."
    );
    Ok(())
}

fn update_command(
    args: &[String],
    presenter: &mut presenter::CatPresenter,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if args.get(1).is_some_and(|argument| argument == "--check") {
        return update_check_command(args);
    }

    println!("GitRun update: fetching and verifying the release manifest");
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    let manifest = if let Some(url) = args.get(1) {
        fetch_manifest(url)?
    } else {
        latest_manifest(&repository)?
    };
    let current = current_version();
    let target = target_triple()?;
    let plan = build_plan(&manifest, &current, &target, &dependency_snapshot())?;
    presenter.transition(presenter::ValidationState::Loading);

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

    let system_install = system_install_paths();
    let config_path = update_config_path(system_install.is_some());
    if system_install.is_some() && config_path.is_none() {
        return Err(
            "trusted system config /etc/gitrun/gitrun.env is missing or unsafe; refusing privileged update"
                .into(),
        );
    }

    let work_root = match system_install {
        Some(_) => PathBuf::from("/var/lib/gitrun/update"),
        None => PathBuf::from(
            std::env::var("GITRUN_UPDATE_DIR").unwrap_or_else(|_| ".gitrun-update".into()),
        ),
    };
    println!("GitRun update: preparing transaction and backup locations");
    std::fs::create_dir_all(&work_root)?;
    let archive = work_root.join(&plan.artifact);
    let artifact = manifest.artifact_for(&target)?;
    println!("GitRun update: downloading release artifact and verifying SHA-256");
    download_and_verify(&plan.artifact_url, &artifact.sha256, &archive)?;
    presenter.transition(presenter::ValidationState::Working);
    println!("GitRun update: artifact integrity PASS");

    // Do not modify host dependencies until the signed release artifact has
    // been downloaded completely and its checksum has been verified.
    println!("GitRun update: checking required host dependencies");
    let updated_dependencies = update_incompatible_dependencies(&plan)?;
    if !updated_dependencies.is_empty() {
        println!("dependencies updated: {}", updated_dependencies.join(", "));
    }

    if let Some((gitrun_binary, version_file)) = system_install {
        let mut should_run_gtuu = plan.runner_image.is_some();
        println!("GitRun update: applying the verified binary transaction");
        // System-update backup destinations are fixed even when running as root.
        let backup_root = PathBuf::from("/var/lib/gitrun/backups");
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

        // Install/repair the persistent privileged control plane as part of
        // a system upgrade. The user UI stays unprivileged; no per-operation
        // sudo prompt is needed after the service is active.
        let desktop_uid = std::env::var("PKEXEC_UID")
            .or_else(|_| std::env::var("SUDO_UID"))
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|uid| *uid != 0);
        println!("GitRun update: reconciling systemd units, control group and GitRun services");
        if let Err(error) =
            gitrun_setup::ensure_control_manager_service_with_binary(desktop_uid, &gitrun_binary)
        {
            let rollback_error = rollback_installed_update_with_config(
                &backup,
                &artifacts,
                &version_file,
                config_path.as_deref(),
                &backup_root,
            )
            .err();
            let restart_error = restart_scheduler_service().err();
            return Err(format!(
                "Control Manager installation failed: {error}; rollback: {}; scheduler recovery: {}",
                rollback_error.map_or_else(|| "not needed".to_owned(), |e| e.to_string()),
                restart_error.map_or_else(|| "not needed".to_owned(), |e| e.to_string())
            ).into());
        }

        if let Some(image) = plan
            .runner_image
            .as_ref()
            .filter(|image| image.reference.contains('/'))
        {
            if let Err(error) = update_runner_image(image) {
                let rollback_error = rollback_installed_update_with_config(
                    &backup,
                    &artifacts,
                    &version_file,
                    config_path.as_deref(),
                    &backup_root,
                )
                .err();
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
                    let rollback_error = rollback_installed_update_with_config(
                        &backup,
                        &artifacts,
                        &version_file,
                        config_path.as_deref(),
                        &backup_root,
                    )
                    .err();
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

        // Local images are rebuilt from the Dockerfile and full GSR build context
        // embedded in this verified release. Build failure is handled before the
        // scheduler restart and never replaces the currently working image tag.
        let local_runner_image = match plan.runner_image.as_ref() {
            Some(image) if !image.reference.contains('/') => Some(image.reference.clone()),
            Some(_) => None,
            None => {
                let configured_image = load_config()?.runner_image;
                (!configured_image.contains('/')).then_some(configured_image)
            }
        };
        if let Some(image_ref) = local_runner_image {
            match gitrun_setup::refresh_local_runner_image_if_stale(&image_ref) {
                Ok(refreshed) => {
                    if refreshed {
                        should_run_gtuu = true;
                        println!("GitRun update: local runner image rebuilt and validated from the release build inputs");
                    } else {
                        println!("GitRun update: local runner image build fingerprint is current");
                    }
                }
                Err(error) => {
                    let rollback_error = rollback_installed_update_with_config(
                        &backup,
                        &artifacts,
                        &version_file,
                        config_path.as_deref(),
                        &backup_root,
                    )
                    .err();
                    return Err(match rollback_error {
                        Some(rollback_error) => format!(
                            "local runner image refresh failed: {error}; GitRun rollback also failed: {rollback_error}"
                        ),
                        None => format!(
                            "local runner image refresh failed; GitRun was rolled back: {error}"
                        ),
                    }
                    .into());
                }
            }
        }

        presenter.transition(presenter::ValidationState::Running);
        println!("GitRun update: restarting and verifying the scheduler");
        if let Err(error) = restart_scheduler_service() {
            let rollback_result = rollback_installed_update_with_config(
                &backup,
                &artifacts,
                &version_file,
                config_path.as_deref(),
                &backup_root,
            );
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

        if should_run_gtuu {
            println!("GitRun update: invoking GTUU to reconcile permanent runners");
            let gtuu_result = (|| -> Result<u32, Box<dyn std::error::Error>> {
                let config = load_config()?;
                let _gtuu_lock =
                    gitrun_scheduler::gtuu::GtuuLock::acquire(Path::new(&config.state_dir))?;
                gitrun_scheduler::run_gtuu_once()
            })();
            match gtuu_result {
                Ok(updated) => println!(
                    "GitRun update: GTUU reconciled {updated} permanent runner(s); busy runners remain untouched"
                ),
                Err(error) => {
                    return Err(format!(
                        "GitRun was installed and the scheduler restarted, but GTUU runner reconciliation failed: {error}. Retry sudo gitrun update --only-containers after resolving the reported issue. Failed image builds preserve the prior image tag and existing containers."
                    )
                    .into());
                }
            }
        }

        if force {
            eprintln!(
                "GitRun update: WARNING — --force requested; skipping only the post-update doctor gate. Manifest signature, artifact SHA-256, executable validation, systemd reconciliation, GTUU reconciliation and scheduler restart checks remain enforced."
            );
        } else if let Err(error) = verify_post_update_doctor(&gitrun_binary) {
            let rollback_result = rollback_installed_update_with_config(
                &backup,
                &artifacts,
                &version_file,
                config_path.as_deref(),
                &backup_root,
            );
            let restore_result = restart_scheduler_service();
            return match (rollback_result, restore_result) {
                (Ok(()), Ok(())) => Err(format!(
                    "post-update doctor failed; GitRun was rolled back and the previous service restarted: {error}"
                ).into()),
                (Err(rollback_error), Ok(())) => Err(format!(
                    "post-update doctor failed: {error}; rollback also failed: {rollback_error}"
                ).into()),
                (Ok(()), Err(restore_error)) => Err(format!(
                    "post-update doctor failed and the rolled-back service could not restart: {error}; restore restart failed: {restore_error}"
                ).into()),
                (Err(rollback_error), Err(restore_error)) => Err(format!(
                    "post-update doctor failed: {error}; rollback failed: {rollback_error}; previous service restart failed: {restore_error}"
                ).into()),
            };
        }

        presenter.transition(presenter::ValidationState::Success);
        if force {
            println!("GitRun update: PASS (binary and systemd verified; post-update doctor skipped by --force)");
        } else {
            println!("GitRun update: PASS (binary, systemd and doctor verified)");
        }
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

    if let Some(image) = plan
        .runner_image
        .as_ref()
        .filter(|image| image.reference.contains('/'))
    {
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
    presenter.transition(presenter::ValidationState::Success);
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

fn verify_post_update_doctor(binary: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = std::process::Command::new(binary)
        // Global CLI flags must precede the subcommand for Clap to parse them.
        .args(["--no-cat", "doctor"])
        .env("GITRUN_CONFIG_FILE", TRUSTED_SYSTEM_CONFIG_FILE)
        .env_remove("GITRUN_CONFIG_DIR")
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return Err(format!(
            "gitrun doctor exited with {}: {}{}",
            output.status,
            stderr,
            if stdout.is_empty() {
                String::new()
            } else {
                format!("; {stdout}")
            }
        )
        .into());
    }
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

fn read_privileged_setup_payload(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let path = Path::new(path);
        let before = std::fs::symlink_metadata(path)?;
        if before.file_type().is_symlink() || !before.is_file() {
            return Err("setup payload must be a regular, non-symlink file".into());
        }
        if before.len() > 1024 * 1024 {
            return Err("setup payload exceeds the 1 MiB safety limit".into());
        }

        let expected_uid = match std::env::var("PKEXEC_UID").or_else(|_| std::env::var("SUDO_UID"))
        {
            Ok(value) => value.parse::<u32>()?,
            Err(_) => 0,
        };
        let mut file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        // Compare the opened inode with lstat's result to reject path swaps
        // between validation and open, then enforce owner/mode on the handle.
        if metadata.dev() != before.dev() || metadata.ino() != before.ino() {
            return Err("setup payload changed during validation".into());
        }
        if !metadata.is_file() || metadata.uid() != expected_uid {
            return Err("setup payload is not owned by the requesting user".into());
        }
        if metadata.mode() & 0o077 != 0 {
            return Err("setup payload permissions must be 0600 or stricter".into());
        }
        if metadata.len() > 1024 * 1024 {
            return Err("setup payload exceeds the 1 MiB safety limit".into());
        }

        let mut raw = String::with_capacity(metadata.len() as usize);
        file.read_to_string(&mut raw)?;
        if raw.len() as u64 > 1024 * 1024 {
            return Err("setup payload exceeds the 1 MiB safety limit".into());
        }
        Ok(raw)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("privileged setup payload validation is supported on Unix only".into())
    }
}

fn reinstall_root_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = read_privileged_setup_payload(path)?;
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

    gitrun_setup::reinstall_linux_with_auth(auth, &repositories, &executable, owner_uid)?;

    println!("GitRun reinstall: PASS");
    Ok(())
}

fn install_root_command(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let raw = read_privileged_setup_payload(path)?;
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

    let runner_profile = values
        .get("RUNNER_PROFILE")
        .map(String::as_str)
        .unwrap_or("workbench");
    if !matches!(runner_profile, "minimum" | "workbench") {
        return Err(format!("unsupported runner profile: {runner_profile}").into());
    }

    bootstrap_linux_with_auth_and_profile(
        auth,
        &repositories,
        &executable,
        owner_uid,
        runner_profile,
    )?;

    initialize_update_signing_environment(true);
    let installed_config = Config::from_env_file(TRUSTED_SYSTEM_CONFIG_FILE)?;
    match refresh_json_setup_assets(&installed_config, true) {
        Ok((backups, library)) => println!("GitRun JSON library: {backups} local backup(s) created; {library} Main Library file(s) added"),
        Err(error) => eprintln!("GitRun setup: WARN — Main Library refresh needs retry: {error}"),
    }
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

fn atomic_replace_text(
    path: &Path,
    content: &str,
    mode: u32,
    prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    #[cfg(not(unix))]
    let _ = mode;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = parent.join(format!(".{prefix}-{}-{nonce}.tmp", std::process::id()));

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }

        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_version_file(path: &Path, version: &str) -> Result<(), Box<dyn std::error::Error>> {
    atomic_replace_text(path, &format!("{version}\n"), 0o644, "gitrun-version")
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
    atomic_replace_text(path, &rendered, 0o600, "gitrun-connect")
}

fn terminal_setup_command(
    presenter: &mut presenter::CatPresenter,
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

    // Do not render mascot transitions between interactive prompts: in an
    // SSH terminal they can appear exactly where the next answer is expected.
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
        presenter.transition(presenter::ValidationState::Working);
        let executable = std::env::current_exe()?;
        // The elevated helper owns the terminal while it performs installation.
        // Stop the inline renderer first and explicitly disable the child mascot.
        presenter.prepare_for_external_process();
        println!();
        println!("Installing GitRun with elevated privileges...");
        let uid = std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()?;
        let running_as_root =
            uid.status.success() && String::from_utf8_lossy(&uid.stdout).trim() == "0";
        let status = if running_as_root {
            std::process::Command::new(&executable)
                .arg("--no-cat")
                .arg("--install-root")
                .arg(&path)
                .status()?
        } else {
            std::process::Command::new("/usr/bin/sudo")
                .arg(&executable)
                .arg("--no-cat")
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
    presenter: &mut presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
    if repository.split('/').count() != 2 || repository.split('/').any(|part| part.is_empty()) {
        return Err("usage: gitrun connect <owner/repository>".into());
    }

    // On a protected system install, repository connection is routed through
    // the persistent Control Manager. This keeps the credential-bearing config
    // root-only and avoids a per-connect sudo/pkexec prompt.
    if gitrun_dashboard_tauri_lib::control_manager_enabled() && !running_as_root() {
        let result = gitrun_dashboard_tauri_lib::control_manager_request(
            "write",
            "connect_repository",
            serde_json::json!({ "repository": repository }),
        )?;
        presenter.transition(presenter::ValidationState::Success);
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let path = persistent_config_path()
        .ok_or("persistent GitRun configuration not found; run 'gitrun setup --terminal' first")?;
    let config = Config::from_env_file(&path)?;
    let auth = GitHubAuth::from_config_file(&config, &path)?;

    verify_repository_access(&auth, repository)?;
    presenter.transition(presenter::ValidationState::Working);

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

    presenter.transition(presenter::ValidationState::Success);
    println!("Repository connected: {repository}");
    println!("Configuration updated: {}", path.display());
    println!("Restart GitRun to apply it to the scheduler.");
    Ok(())
}

fn rollback_command(
    path: &str,
    presenter: &mut presenter::CatPresenter,
) -> Result<(), Box<dyn std::error::Error>> {
    presenter.transition(presenter::ValidationState::Recovering);
    let raw = std::fs::read_to_string(path)?;
    if let Ok(backup) = serde_json::from_str::<InstalledBackupRecord>(&raw) {
        let (gitrun_binary, version_file) = system_install_paths().ok_or(
            "installed-system rollback records can only be applied by the installed GitRun binary",
        )?;
        let backup_root = PathBuf::from("/var/lib/gitrun/backups");
        let expected_artifacts = [InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: gitrun_binary,
            required: true,
        }];
        rollback_installed_update_with_config(
            &backup,
            &expected_artifacts,
            &version_file,
            Some(Path::new(TRUSTED_SYSTEM_CONFIG_FILE)),
            &backup_root,
        )?;
        presenter.transition(presenter::ValidationState::Success);
        println!("GitRun rollback: PASS");
        return Ok(());
    }

    // System installs exclusively use InstalledBackupRecord with fixed trusted
    // artifact destinations. Never reinterpret a malformed/system record as a
    // developer backup whose JSON could select arbitrary restore paths.
    if system_install_paths().is_some() {
        return Err(
            "invalid or unsupported system rollback record; refusing untrusted restore paths"
                .into(),
        );
    }

    let backup: BackupRecord = serde_json::from_str(&raw)?;
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
    rollback(&paths, &backup)?;
    presenter.transition(presenter::ValidationState::Success);
    println!("GitRun rollback: PASS");
    Ok(())
}

fn validate_version_flag_order(presenter: &mut presenter::CatPresenter) -> bool {
    let mut version_seen = false;

    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--version" | "-V" | "-v" => version_seen = true,
            "--gitrun" if !version_seen => {
                presenter.transition(presenter::ValidationState::Failure);
                eprintln!("error: --gitrun must follow -V, -v, or --version");
                return false;
            }
            _ => {}
        }
    }

    true
}

fn no_cat_override_requested(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    arguments
        .into_iter()
        .any(|argument| argument == std::ffi::OsStr::new("--no-cat"))
}

#[cfg(unix)]
fn run_gsr_watchdog() -> i32 {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    let stopping = Arc::new(AtomicBool::new(false));
    if let Err(error) = signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())
    {
        std::eprintln!("gitrun gsr-watchdog: failed to register SIGTERM handler: {error}");
        return 1;
    }
    if let Err(error) = signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone()) {
        std::eprintln!("gitrun gsr-watchdog: failed to register SIGINT handler: {error}");
        return 1;
    }

    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            std::eprintln!("gitrun gsr-watchdog: invalid configuration: {error}");
            return 2;
        }
    };
    let watch_config = gitrun_gsr::WatchConfig {
        pid_file: PathBuf::from(&config.state_dir).join("gitrun-autoscaler.pid"),
        events_path: gitrun_gsr::events::default_queue_path(&config.state_dir),
        poll_interval: Duration::from_secs(5),
        watched_name: "gitrun-autoscaler".into(),
    };

    std::println!(
        "gitrun-gsr: watching {} (pid file: {})",
        watch_config.watched_name,
        watch_config.pid_file.display()
    );
    gitrun_gsr::run_watchdog(&watch_config, || stopping.load(Ordering::Relaxed));
    std::println!("gitrun-gsr: stopped");
    0
}

#[cfg(not(unix))]
fn run_gsr_watchdog() -> i32 {
    std::eprintln!("gitrun gsr-watchdog is currently supported on Unix hosts only");
    2
}

fn is_interactive_terminal_setup(command: &Command) -> bool {
    matches!(command, Command::Setup { terminal: true })
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("gsr-watchdog")) {
        std::process::exit(run_gsr_watchdog());
    }
    // The persistent Control Manager is a systemd-only internal entry point. It
    // bypasses the interactive presenter and refuses to run without root.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("control-manager")) {
        std::process::exit(gitrun_dashboard_tauri_lib::run_control_manager());
    }
    // Initialize the shared presenter before Clap so help and argument errors
    // follow the same mascot policy as normal commands. Internal callers can
    // still explicitly suppress it with --no-cat.
    let no_cat_override = no_cat_override_requested(std::env::args_os().skip(1));
    let mut presenter = presenter::CatPresenter::new(no_cat_override);

    if !validate_version_flag_order(&mut presenter) {
        std::process::exit(2);
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    presenter.transition(presenter::ValidationState::Ready);
                }
                _ => presenter.transition(presenter::ValidationState::Failure),
            }
            let _ = error.print();
            std::process::exit(exit_code);
        }
    };
    debug_assert_eq!(cli.no_cat, no_cat_override);

    if cli.gitrun {
        presenter.start_live();
        presenter.transition(presenter::ValidationState::Ready);
        println!("GitRun {}", current_version());
        presenter.finish(0);
        std::process::exit(0);
    }
    if cli.version || cli.all_crates || cli.crate_name.is_some() {
        presenter.start_live();
        presenter.transition(presenter::ValidationState::Ready);
        let exit_code = run_version(cli.crate_name.as_deref());
        presenter.finish(exit_code);
        std::process::exit(exit_code);
    }

    let command = cli.command.unwrap_or(Command::Dashboard);
    let is_cat_command = matches!(&command, Command::Cat);
    let is_status_command = matches!(&command, Command::Status { .. });
    let is_manage_command = matches!(&command, Command::Manage);
    let is_doctor_command = matches!(&command, Command::Doctor);
    let is_interactive_setup = is_interactive_terminal_setup(&command);
    // Interactive setup reads credentials and paths from stdin. Keep the live
    // mascot viewport out of that terminal so it cannot overwrite or obscure
    // the prompt where the user is typing.
    // Doctor emits a multi-check report. Keep its output in normal terminal
    // scrollback instead of allowing the animated inline viewport to redraw it.
    if !is_cat_command
        && !is_status_command
        && !is_manage_command
        && !is_interactive_setup
        && !is_doctor_command
    {
        presenter.start_live();
    }
    if !is_cat_command && !is_manage_command && !is_interactive_setup && !is_doctor_command {
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
        Command::FastStop {
            repository,
            run_id,
            yes: _,
        } => run_fast_stop(&repository, run_id),
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
        Command::Uninstall => run_embedded_privileged_helper(
            "gitrun-uninstall",
            include_str!("../../../scripts/uninstall.sh"),
            &mut presenter,
        ),
        Command::Reinstall => run_embedded_privileged_helper(
            "gitrun-reinstall",
            include_str!("../../../scripts/reinstall-latest.sh"),
            &mut presenter,
        ),
        Command::Update {
            manifest_url,
            check: true,
            ..
        } => run_update_check(manifest_url.as_deref()),
        Command::Update {
            manifest_url,
            only_containers,
            check: false,
            force,
        } => run_update(
            manifest_url.as_deref(),
            only_containers,
            force,
            &mut presenter,
        ),
        Command::Gtuu {
            check,
            rebuild,
            repo,
            all,
            force,
        } => run_gtuu_command(
            repo.as_deref(),
            all,
            rebuild,
            force || rebuild,
            check,
            &mut presenter,
        ),
        Command::FixUpdate => run_fix_update(&mut presenter),
        Command::FixUpdateRoot => run_fix_update(&mut presenter),
        Command::Scheduler => run_scheduler(),
        Command::Dashboard => {
            // The Tauri runtime may write native GTK/EGL diagnostics directly to
            // stderr. Do not keep the Ratatui inline renderer active while the
            // graphical dashboard owns the process and terminal.
            presenter.prepare_for_external_process();
            run_dashboard()
        }
        Command::Manage => {
            presenter.prepare_for_external_process();
            match manage::run() {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("GitRun management console: FAIL — {error}");
                    1
                }
            }
        }
        Command::RecoveryGtuu => run_recovery_gtuu(&mut presenter),
        Command::RepairService => run_repair_service(),
        Command::InstallRoot { token_path } => run_install_root(&token_path),
        Command::UninstallRoot => run_uninstall_root(false),
        Command::UninstallPackageHook => run_uninstall_root(true),
        Command::ReinstallRoot { token_path } => run_reinstall_root(&token_path),
        Command::ReinstallLatestRoot => match reinstall_latest_root_command() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun reinstall: FAIL — {error}");
                1
            }
        },
        Command::ReinstallExistingRoot => match reinstall_existing_root_command() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun reinstall: runtime rebuild failed — {error}");
                1
            }
        },
        Command::ReadVmConfigsRoot => match read_vm_configs_root_command() {
            Ok(()) => 0,
            Err(error) => {
                std::eprintln!("GitRun VM configuration read: FAIL — {error}");
                1
            }
        },
        Command::SaveVmConfigsRoot { input_path } => {
            match save_vm_configs_root_command(&input_path) {
                Ok(()) => 0,
                Err(error) => {
                    std::eprintln!("GitRun VM configuration save: FAIL — {error}");
                    1
                }
            }
        }
        Command::ReadPendingHypervisorDecisionsRoot => {
            match read_pending_hypervisor_decisions_root_command() {
                Ok(()) => 0,
                Err(error) => {
                    std::eprintln!("GitRun hypervisor decision read: FAIL — {error}");
                    1
                }
            }
        }
        Command::RespondHypervisorDecisionRoot { vm_name, choice } => {
            match respond_hypervisor_decision_root_command(&vm_name, &choice) {
                Ok(()) => 0,
                Err(error) => {
                    std::eprintln!("GitRun hypervisor decision response: FAIL — {error}");
                    1
                }
            }
        }
        Command::ReadLogicRulesRoot => match read_logic_rules_root_command() {
            Ok(()) => 0,
            Err(error) => {
                std::eprintln!("GitRun Logic Containers read: FAIL — {error}");
                1
            }
        },
        Command::SaveLogicRulesRoot { input_path } => {
            match save_logic_rules_root_command(&input_path) {
                Ok(()) => 0,
                Err(error) => {
                    std::eprintln!("GitRun Logic Containers save: FAIL — {error}");
                    1
                }
            }
        }
        Command::ReadDockRequirementsRoot { repository } => {
            match read_dock_requirements_root_command(&repository) {
                Ok(()) => 0,
                Err(error) => {
                    std::eprintln!("GitRun GitDockRun requirements read: FAIL — {error}");
                    1
                }
            }
        }
        Command::ReadGsrEventsRoot { limit } => match read_gsr_events_root_command(limit) {
            Ok(()) => 0,
            Err(error) => {
                std::eprintln!("GitRun GSR event read: FAIL — {error}");
                1
            }
        },
        Command::SaveConfigRoot { input_path } => match save_config_root_command(&input_path) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun settings save: FAIL — {error}");
                1
            }
        },
        Command::SaveGitRunSettingsRoot { input_path } => {
            match save_gitrun_settings_root_command(&input_path) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("GitRun security settings save: FAIL — {error}");
                    1
                }
            }
        }
        Command::JsonRecoveryRoot { action } => {
            let system_install = system_install_paths().is_some();
            initialize_update_signing_environment(system_install);
            initialize_system_update_config_path(system_install);
            match gitrun_recovery::run_json_recovery_root_action(&action) {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string(&report).unwrap_or_else(|_| "{}".into())
                    );
                    0
                }
                Err(error) => {
                    eprintln!("GitRun JSON recovery: FAIL — {error}");
                    1
                }
            }
        }
        Command::ReadGitRunSettingsRoot => match read_gitrun_settings_root_command() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun security settings read: FAIL — {error}");
                1
            }
        },
        Command::ReadKittyDashboardRoot => match read_kitty_dashboard_root_command() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("GitRun Kitty dashboard read: FAIL — {error}");
                1
            }
        },
        Command::SaveKittySettingsRoot { input_path } => {
            match save_kitty_settings_root_command(&input_path) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("GitRun Kitty settings save: FAIL — {error}");
                    1
                }
            }
        }
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

    if !is_cat_command && !is_status_command && !is_manage_command {
        // Doctor keeps the report in scrollback, then prints the mascot as a
        // final status line instead of running an inline viewport over output.
        presenter.finish(exit_code);
    }
    std::process::exit(exit_code);
}

fn validation_state_for_command(command: &Command) -> presenter::ValidationState {
    use presenter::ValidationState;

    match command {
        Command::Config | Command::Settings => ValidationState::Reading,
        Command::Desired { .. } => ValidationState::Working,
        Command::FastStop { .. } => ValidationState::Working,
        Command::Setup { .. } | Command::InstallRoot { .. } => ValidationState::Setup,
        Command::Cat => ValidationState::Ready,
        Command::Connect { .. } => ValidationState::Connecting,
        Command::Doctor => ValidationState::Validating,
        // Update validates the release plan before entering the updating state.
        // Keeping the initial state neutral avoids showing "updating" when the
        // requested release is already installed or otherwise rejected.
        Command::Update { .. } | Command::Gtuu { .. } | Command::FixUpdate => {
            ValidationState::Ready
        }
        Command::Scheduler | Command::Dashboard => ValidationState::Running,
        Command::Manage => ValidationState::Reading,
        Command::Uninstall
        | Command::Reinstall
        | Command::UninstallRoot
        | Command::UninstallPackageHook
        | Command::ReinstallRoot { .. }
        | Command::ReinstallLatestRoot
        | Command::ReinstallExistingRoot
        | Command::ReadVmConfigsRoot
        | Command::SaveVmConfigsRoot { .. }
        | Command::ReadPendingHypervisorDecisionsRoot
        | Command::RespondHypervisorDecisionRoot { .. }
        | Command::ReadLogicRulesRoot
        | Command::SaveLogicRulesRoot { .. }
        | Command::ReadDockRequirementsRoot { .. }
        | Command::ReadGsrEventsRoot { .. }
        | Command::SaveConfigRoot { .. }
        | Command::SaveGitRunSettingsRoot { .. }
        | Command::SaveKittySettingsRoot { .. }
        | Command::ReadKittyDashboardRoot
        | Command::ReadGitRunSettingsRoot
        | Command::JsonRecoveryRoot { .. }
        | Command::FixUpdateRoot => ValidationState::Recovering,
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
    /// Disable all cat rendering for this process. Internal use for
    /// delegated processes that must own the TTY without Ratatui output.
    #[arg(long = "no-cat", hide = true, global = true)]
    no_cat: bool,

    /// Print the GitRun version and exit. Accepts both `-V` (Unix
    /// convention, e.g. `gcc -V`, `rustc -V`) and `-v` as a short form,
    /// since operators reach for either out of habit and there's no other
    /// use for `-v` on this top-level flag set to conflict with.
    #[arg(short = 'V', long = "version", short_alias = 'v', alias = "v")]
    version: bool,
    /// Print only the GitRun program version, without workspace crate versions.
    #[arg(
        long = "gitrun",
        requires = "version",
        conflicts_with_all = ["crate_name", "all_crates"]
    )]
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
    /// Immediately force-cancel an entire GitHub Actions workflow run (all jobs in that run).
    FastStop {
        /// Repository in owner/repository form; must be configured in GitRun.
        repository: String,
        /// GitHub Actions workflow run ID.
        run_id: u64,
        /// Required acknowledgement that the entire workflow run will be cancelled.
        #[arg(long, required = true)]
        yes: bool,
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
        /// Check the current release and runner image without downloading or applying anything.
        #[arg(long, conflicts_with = "only_containers")]
        check: bool,
        /// Update only the permanent runner pool without replacing GitRun itself.
        #[arg(long, conflicts_with = "check")]
        only_containers: bool,
        /// Skip the post-update doctor gate. Signed-manifest, checksum, executable and service checks remain enabled.
        #[arg(long, conflicts_with_all = ["check", "only_containers"])]
        force: bool,
        /// Optional manifest URL to inspect instead of the latest GitHub release.
        manifest_url: Option<String>,
    },
    /// Inspect or repair GitRun-managed permanent runner containers.
    Gtuu {
        /// Read-only image and permanent-container inventory.
        #[arg(long, conflicts_with_all = ["rebuild", "force"])]
        check: bool,
        /// Force a fresh, validated local runner image build before reconciliation.
        #[arg(long, conflicts_with = "check")]
        rebuild: bool,
        /// Restrict the operation to one configured owner/repository.
        #[arg(long, conflicts_with = "all")]
        repo: Option<String>,
        /// Explicitly select every configured repository.
        #[arg(long)]
        all: bool,
        /// Replace selected permanent runners even when image IDs match.
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
    /// Repair the system update integration and then apply the latest official release if newer.
    FixUpdate,
    /// Internal privileged entry point for `fix-update`.
    #[command(name = "--fix-update-root", hide = true)]
    FixUpdateRoot,
    /// Remove GitRun services, runtime resources, package registration and app data.
    Uninstall,
    /// Download, verify and reinstall the latest official GitRun release.
    Reinstall,
    /// Internal privileged uninstall entry point.
    #[command(name = "--uninstall-root", hide = true)]
    UninstallRoot,
    /// Internal apt maintainer hook; it must never invoke dpkg itself.
    #[command(name = "--uninstall-package-hook", hide = true)]
    UninstallPackageHook,

    /// Internal privileged command that downloads and installs a signed official .deb.
    #[command(name = "--reinstall-latest-root", hide = true)]
    ReinstallLatestRoot,
    /// Internal command that rebuilds runtime resources from the saved configuration.
    #[command(name = "--reinstall-existing-root", hide = true)]
    ReinstallExistingRoot,
    /// Internal read-only helper for root-owned VM configuration.
    #[command(name = "--read-vm-configs-root", hide = true)]
    ReadVmConfigsRoot,
    /// Internal validated writer for root-owned VM configuration.
    #[command(name = "--save-vm-configs-root", hide = true)]
    SaveVmConfigsRoot {
        /// Private JSON payload created by the unprivileged dashboard.
        input_path: String,
    },
    /// Internal read-only helper for pending hypervisor decisions.
    #[command(name = "--read-pending-hypervisor-decisions-root", hide = true)]
    ReadPendingHypervisorDecisionsRoot,
    /// Internal helper to answer one pending hypervisor decision.
    #[command(name = "--respond-hypervisor-decision-root", hide = true)]
    RespondHypervisorDecisionRoot {
        /// VM name from the pending decision record.
        vm_name: String,
        /// One of retry_kvm or use_virtual_box.
        choice: String,
    },
    /// Internal read-only helper for Logic Containers policy data.
    #[command(name = "--read-logic-rules-root", hide = true)]
    ReadLogicRulesRoot,
    /// Internal helper to save validated Logic Containers policy through an authenticated payload.
    #[command(name = "--save-logic-rules-root", hide = true)]
    SaveLogicRulesRoot {
        /// Private JSON payload created by the unprivileged dashboard.
        input_path: String,
    },
    /// Internal read-only helper for requirements of a configured repository.
    #[command(name = "--read-dock-requirements-root", hide = true)]
    ReadDockRequirementsRoot {
        /// Exact owner/repository value from the trusted system configuration.
        repository: String,
    },
    /// Internal read-only helper for the dashboard; reads only the trusted GSR queue.
    #[command(name = "--read-gsr-events-root", hide = true)]
    ReadGsrEventsRoot {
        /// Maximum number of recent events to return (clamped to 500).
        limit: usize,
    },
    /// Internal command to persist validated settings to the system env file.
    #[command(name = "--save-config-root", hide = true)]
    SaveConfigRoot {
        /// Private JSON file containing a Config object.
        input_path: String,
    },
    /// Internal privileged command to atomically persist the security policy.
    #[command(name = "--save-gitrun-settings-root", hide = true)]
    SaveGitRunSettingsRoot {
        /// Private JSON file containing a GitRunSettings object.
        input_path: String,
    },
    /// Internal fixed-purpose privileged JSON recovery action.
    #[command(name = "--json-recovery-root", hide = true)]
    JsonRecoveryRoot {
        /// One of repair, restore-backups, or restore-main-library.
        action: String,
    },
    /// Internal read-only helper for root-protected GitRun security settings.
    #[command(name = "--read-gitrun-settings-root", hide = true)]
    ReadGitRunSettingsRoot,
    /// Internal read-only helper for the protected Kitty dashboard state.
    #[command(name = "--read-kitty-dashboard-root", hide = true)]
    ReadKittyDashboardRoot,
    /// Internal privileged command to persist validated optional Kitty settings.
    #[command(name = "--save-kitty-settings-root", hide = true)]
    SaveKittySettingsRoot {
        /// Private JSON file containing a KittySettings object.
        input_path: String,
    },

    /// Internal privileged reinstall entry point.
    #[command(name = "--reinstall-root", hide = true)]
    ReinstallRoot { token_path: String },

    /// Internal scheduler service entry point.
    #[command(name = "scheduler", hide = true)]
    Scheduler,
    /// Launch the GitRun dashboard (default when no command is given).
    Dashboard,
    /// Launch the interactive terminal management console for headless hosts.
    Manage,
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

    if let Some(name) = crate_name {
        let wanted = if name == "gitrun" {
            "gitrun".to_owned()
        } else if name.starts_with("gitrun-") {
            name.to_owned()
        } else {
            format!("gitrun-{name}")
        };

        if wanted == "gitrun" {
            println!("GitRun {version}");
        } else if let Some((_, crate_version)) = GITRUN_CRATE_VERSIONS
            .iter()
            .find(|(crate_name, _)| *crate_name == wanted)
        {
            println!("{wanted} {crate_version}");
        } else {
            eprintln!("unknown GitRun crate: {name}");
            return 2;
        }
    } else {
        println!("GitRun {version}");
        for (name, crate_version) in GITRUN_CRATE_VERSIONS {
            println!("{name} {crate_version}");
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

fn run_fast_stop(repository: &str, run_id: u64) -> i32 {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("GitRun fast-stop: could not load configuration: {error}");
            return 2;
        }
    };
    if !config
        .repositories
        .iter()
        .any(|configured| configured == repository)
    {
        eprintln!("GitRun fast-stop: refusing repository {repository:?}; it is not in the configured repository allowlist");
        return 2;
    }
    if run_id == 0 {
        eprintln!("GitRun fast-stop: workflow run ID must be greater than zero");
        return 2;
    }
    let client = match build_status_github_client(&config) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("GitRun fast-stop: GitHub authentication failed: {error}");
            return 1;
        }
    };
    match client.force_cancel_workflow_run(repository, run_id) {
        Ok(()) => {
            println!("GitRun fast-stop: GitHub accepted force-cancellation for workflow run {run_id} in {repository}. All jobs in that workflow run are affected.");
            0
        }
        Err(error) => {
            eprintln!("GitRun fast-stop: force-cancellation failed for workflow run {run_id} in {repository}: {error}");
            1
        }
    }
}

fn runner_validation_state(
    runner: &gitrun_scheduler::Runner,
    container: Option<&gitrun_scheduler::docker::ManagedContainer>,
) -> presenter::ValidationState {
    use presenter::ValidationState;

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
    use presenter::ValidationState;

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

fn run_status(runner_id: u64, presenter: &mut presenter::CatPresenter) -> i32 {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            presenter.show_named("failed");
            eprintln!("configuration error: {error}");
            return 2;
        }
    };

    presenter.transition(presenter::ValidationState::Api);

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
            gitrun_core::GitRunOperation::Plan,
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

fn run_check_compatibility(workflow: Option<&str>, presenter: &mut presenter::CatPresenter) -> i32 {
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
        presenter.transition(presenter::ValidationState::Testing);
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

fn run_setup(presenter: &mut presenter::CatPresenter) -> i32 {
    match load_config() {
        Ok(config) => {
            let config_dir = setup_config_dir();
            match prepare_directories(&config, config_dir) {
                Ok(report) => {
                    presenter.transition(presenter::ValidationState::Working);
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
                    match refresh_json_setup_assets(&config, system_install_paths().is_some()) {
                        Ok((backups, library)) => println!("GitRun JSON library: {backups} local backup(s) created; {library} Main Library file(s) added"),
                        Err(error) => eprintln!("GitRun setup: WARN — JSON library refresh deferred/failed: {error}"),
                    }
                    presenter.transition(presenter::ValidationState::Success);
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

fn refresh_json_setup_assets(
    config: &Config,
    system_install: bool,
) -> Result<(usize, usize), String> {
    let mut roots = vec![PathBuf::from(config.state_dir.as_str())];
    if Path::new("config").is_dir() {
        roots.push(PathBuf::from("config"));
    }
    if system_install {
        roots.push(PathBuf::from("/etc/gitrun"));
    }
    let backups = gitrun_updater::refresh_json_backups(&roots)
        .map_err(|error| format!("create missing JSON.Back snapshots: {error}"))?;

    let can_write_system_library = !system_install || running_as_root();
    let library_root = if system_install {
        PathBuf::from("/usr/share/gitrun/library")
    } else {
        PathBuf::from("config/gitrun-library")
    };
    let library = if can_write_system_library {
        initialize_update_signing_environment(system_install);
        gitrun_updater::install_main_library_from_latest_release(
            "Vider06/GitRun",
            &library_root,
            false,
        )
        .map_err(|error| format!("install signed Main Library: {error}"))?
    } else {
        0
    };
    Ok((backups, library))
}

fn doctor_check(
    label: &str,
    result: Result<String, String>,
    failures: &mut usize,
    warnings: &mut usize,
) {
    match result {
        Ok(detail) => println!("[PASS] {label}: {detail}"),
        Err(detail) if detail.starts_with("INFO: ") => {
            println!("[INFO] {label}: {}", detail.trim_start_matches("INFO: "));
        }
        Err(detail) if detail.starts_with("WARN: ") => {
            *warnings += 1;
            println!("[WARN] {label}: {}", detail.trim_start_matches("WARN: "));
        }
        Err(detail) => {
            *failures += 1;
            println!("[FAIL] {label}: {detail}");
        }
    }
}

fn doctor_file_check(path: &str, require_root_owned: bool) -> Result<String, String> {
    let path_obj = Path::new(path);
    let metadata =
        std::fs::symlink_metadata(path_obj).map_err(|error| format!("{path}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{path} must not be a symbolic link"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if require_root_owned && metadata.uid() != 0 {
            // Report the failed ownership policy without exposing host account
            // identifiers in CLI output or logs.
            return Err(format!("{path} is not owned by root"));
        }
        if require_root_owned && metadata.mode() & 0o022 != 0 {
            return Err(format!("{path} is writable by group or others"));
        }
    }
    Ok(format!(
        "present ({})",
        if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        }
    ))
}

fn doctor_command_status(program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(if detail.is_empty() {
            "OK".into()
        } else {
            detail
        })
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if detail.is_empty() {
            format!("{program} exited with {}", output.status)
        } else {
            detail
        })
    }
}

fn doctor_service_check(service: &str) -> Result<String, String> {
    match doctor_command_status("/usr/bin/systemctl", &["is-active", service]) {
        Ok(state) if state == "active" => Ok("active".into()),
        Ok(state) => Err(format!("reports {state}; expected active")),
        Err(error) => Err(format!(
            "not active ({error}); inspect systemctl status {service}"
        )),
    }
}

fn doctor_expected_protected_json_permissions() -> bool {
    system_install_paths().is_some()
        && !running_as_root()
        && gitrun_dashboard_tauri_lib::control_manager_enabled()
}

fn doctor_json_tree_check(root: &Path, failures: &mut usize, warnings: &mut usize) {
    if !root.exists() {
        println!(
            "[INFO] JSON scan skipped: {} does not exist",
            root.display()
        );
        return;
    }
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            doctor_check(
                &format!("JSON directory {}", root.display()),
                Err("refusing to follow a symbolic link".into()),
                failures,
                warnings,
            );
            return;
        }
        Ok(metadata) if !metadata.is_dir() => {
            println!(
                "[INFO] JSON scan skipped: {} is not a directory",
                root.display()
            );
            return;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let detail = if doctor_expected_protected_json_permissions() {
                format!(
                    "INFO: protected system directory is not readable by this account; run `sudo gitrun doctor` for full JSON validation ({error})"
                )
            } else {
                format!(
                    "WARN: permission denied while inspecting {}: {error}",
                    root.display()
                )
            };
            doctor_check(
                &format!("JSON directory {}", root.display()),
                Err(detail),
                failures,
                warnings,
            );
            return;
        }
        Err(error) => {
            doctor_check(
                &format!("JSON directory {}", root.display()),
                Err(format!("{}: {error}", root.display())),
                failures,
                warnings,
            );
            return;
        }
    }

    let expected_protected_permissions = doctor_expected_protected_json_permissions();
    let mut pending = vec![root.to_path_buf()];
    let mut checked = 0usize;
    let mut skipped_protected = 0usize;
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && expected_protected_permissions
                {
                    skipped_protected += 1;
                    continue;
                }
                let detail = if error.kind() == std::io::ErrorKind::PermissionDenied {
                    format!(
                        "WARN: permission denied while reading {}: {error}",
                        directory.display()
                    )
                } else {
                    format!("{}: {error}", directory.display())
                };
                doctor_check(
                    &format!("JSON directory {}", directory.display()),
                    Err(detail),
                    failures,
                    warnings,
                );
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    doctor_check(
                        &format!("JSON directory {}", directory.display()),
                        Err(format!("unable to read a directory entry: {error}")),
                        failures,
                        warnings,
                    );
                    continue;
                }
            };
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_json = name.ends_with(".json") || name.ends_with(".json.back");
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    doctor_check(
                        &format!("JSON entry {}", path.display()),
                        Err(format!("could not inspect path: {error}")),
                        failures,
                        warnings,
                    );
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                if is_json {
                    doctor_check(
                        &format!("JSON file {}", path.display()),
                        Err("symbolic links are not followed for JSON validation".into()),
                        failures,
                        warnings,
                    );
                }
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() || !is_json {
                continue;
            }
            let file_result = doctor_json_file_check(&path, expected_protected_permissions);
            if matches!(&file_result, Err(detail) if detail.starts_with("INFO: ")) {
                skipped_protected += 1;
                continue;
            }
            checked += 1;
            doctor_check(
                &format!("JSON {}", path.display()),
                file_result,
                failures,
                warnings,
            );
            if checked >= 10_000 {
                doctor_check(
                    &format!("JSON scan {}", root.display()),
                    Err("scan reached the 10,000-file safety limit".into()),
                    failures,
                    warnings,
                );
                return;
            }
        }
    }
    if skipped_protected > 0 {
        println!(
            "[INFO] JSON scan under {} inspected {checked} file(s) and skipped {skipped_protected} protected path(s); run `sudo gitrun doctor` for full JSON validation",
            root.display()
        );
    } else {
        println!(
            "[INFO] JSON scan complete: inspected {checked} JSON / JSON.Back file(s) under {}",
            root.display()
        );
    }
}

fn doctor_json_file_check(
    path: &Path,
    expected_protected_permissions: bool,
) -> Result<String, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular, non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > 16 * 1024 * 1024 {
        return Err(format!(
            "{} exceeds the 16 MiB JSON safety limit",
            path.display()
        ));
    }
    let content = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied && expected_protected_permissions {
            "INFO: protected by system permissions; run `sudo gitrun doctor` to validate this file"
                .to_owned()
        } else if error.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "WARN: permission denied reading {}: {error}",
                path.display()
            )
        } else {
            format!("{} cannot be read as UTF-8: {error}", path.display())
        }
    })?;
    let value: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
        format!(
            "{} is invalid JSON at line {}, column {}: {error}",
            path.display(),
            error.line(),
            error.column()
        )
    })?;
    if !(value.is_object() || value.is_array()) {
        return Err(format!(
            "{} has a scalar JSON root; configuration JSON must be an object or array",
            path.display()
        ));
    }
    Ok("valid JSON syntax and top-level structure".into())
}

fn doctor_load_config() -> Result<Config, String> {
    let system_install = system_install_paths().is_some();
    if system_install && !running_as_root() && gitrun_dashboard_tauri_lib::control_manager_enabled()
    {
        // The system env file may be root-owned and intentionally unreadable
        // to the desktop account. Reuse the same allowlisted Control Manager
        // read used by the dashboard instead of mistaking that ACL for broken
        // configuration. A direct read remains the explicit fallback.
        match gitrun_dashboard_tauri_lib::management_tui_read(
            "config",
            serde_json::json!({}),
        ) {
            Ok(value) => serde_json::from_value(value)
                .map_err(|error| format!("Control Manager returned invalid configuration: {error}")),
            Err(ipc_error) => load_config().map_err(|error| {
                format!(
                    "configuration could not be read directly ({error}); Control Manager read failed ({ipc_error})"
                )
            }),
        }
    } else {
        load_config().map_err(|error| error.to_string())
    }
}

fn run_doctor() -> i32 {
    #[cfg(unix)]
    use std::os::unix::fs::FileTypeExt;

    println!("GitRun doctor: checking installation, configuration and runtime health");
    let mut failures = 0usize;
    let mut warnings = 0usize;
    let system_install = system_install_paths().is_some();
    if cfg!(target_os = "linux") {
        for (label, program, args) in [
            ("Git executable", "git", vec!["--version"]),
            ("Docker executable", "docker", vec!["--version"]),
        ] {
            let result =
                doctor_command_status(program, &args).map_err(|error| format!("WARN: {error}"));
            doctor_check(label, result, &mut failures, &mut warnings);
        }
        match doctor_command_status("docker", &["info", "--format", "{{.ServerVersion}}"]) {
            Ok(version) => doctor_check(
                "Docker daemon",
                Ok(format!("available ({version})")),
                &mut failures,
                &mut warnings,
            ),
            Err(error) => doctor_check(
                "Docker daemon",
                Err(format!("WARN: {error}")),
                &mut failures,
                &mut warnings,
            ),
        }
    }
    match std::env::current_exe() {
        Ok(path) if path.is_file() => doctor_check(
            "CLI binary",
            Ok(path.display().to_string()),
            &mut failures,
            &mut warnings,
        ),
        Ok(path) => doctor_check(
            "CLI binary",
            Err(format!("{} is not a regular file", path.display())),
            &mut failures,
            &mut warnings,
        ),
        Err(error) => doctor_check(
            "CLI binary",
            Err(format!("cannot resolve executable: {error}")),
            &mut failures,
            &mut warnings,
        ),
    }

    let config_path = if system_install {
        PathBuf::from(TRUSTED_SYSTEM_CONFIG_FILE)
    } else {
        persistent_config_path().unwrap_or_else(|| PathBuf::from("config/gitrun.env"))
    };
    let config_result = if system_install {
        doctor_file_check(TRUSTED_SYSTEM_CONFIG_FILE, true).and_then(|detail| {
            trusted_system_config_path()
                .map(|_| detail)
                .ok_or_else(|| "configuration is unsafe or not root-owned".to_owned())
        })
    } else {
        doctor_file_check(config_path.to_string_lossy().as_ref(), false)
            .map_err(|error| format!("WARN: {error}"))
    };
    doctor_check(
        "Configuration file",
        config_result,
        &mut failures,
        &mut warnings,
    );

    let mut json_roots = vec![
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("config"))
            .to_path_buf(),
        PathBuf::from("config"),
        // Keep checking the standard state tree even if the env file is
        // unreadable or malformed and cannot provide a custom state_dir.
        PathBuf::from("/var/lib/gitrun"),
        PathBuf::from("/usr/share/gitrun/library"),
    ];
    match doctor_load_config() {
        Ok(config) => {
            json_roots.push(PathBuf::from(&config.state_dir));
            if config.repositories.is_empty() {
                doctor_check(
                    "Configuration",
                    Err("no repositories configured".into()),
                    &mut failures,
                    &mut warnings,
                );
            } else {
                doctor_check(
                    "Configuration",
                    Ok(format!(
                        "{} repository/repositories; runner pool {}..{}",
                        config.repositories.len(),
                        config.min_runners,
                        config.max_runners
                    )),
                    &mut failures,
                    &mut warnings,
                );
            }
            if config.min_runners > config.max_runners {
                doctor_check(
                    "Runner pool limits",
                    Err(format!(
                        "minimum {} exceeds maximum {}",
                        config.min_runners, config.max_runners
                    )),
                    &mut failures,
                    &mut warnings,
                );
            } else {
                doctor_check(
                    "Runner pool limits",
                    Ok("minimum/maximum are consistent".into()),
                    &mut failures,
                    &mut warnings,
                );
            }
            for (label, path) in [
                ("State directory", config.state_dir.as_str()),
                ("Log directory", config.log_dir.as_str()),
            ] {
                match std::fs::metadata(path) {
                    Ok(metadata) if metadata.is_dir() => {
                        doctor_check(label, Ok(path.to_owned()), &mut failures, &mut warnings)
                    }
                    Ok(_) => doctor_check(
                        label,
                        Err(format!("{path} exists but is not a directory")),
                        &mut failures,
                        &mut warnings,
                    ),
                    Err(error) => doctor_check(
                        label,
                        Err(format!("WARN: {path}: {error}")),
                        &mut failures,
                        &mut warnings,
                    ),
                }
            }
        }
        Err(error) => doctor_check(
            "Configuration parsing",
            Err(error.to_string()),
            &mut failures,
            &mut warnings,
        ),
    }
    json_roots.sort();
    json_roots.dedup();
    for root in json_roots {
        doctor_json_tree_check(&root, &mut failures, &mut warnings);
    }

    if system_install && cfg!(target_os = "linux") {
        match std::fs::read_to_string("/usr/share/gitrun/version.txt") {
            Ok(version) if version.trim().is_empty() => doctor_check(
                "Installed version metadata",
                Err("version.txt is empty".into()),
                &mut failures,
                &mut warnings,
            ),
            Ok(version) => {
                let version = version.trim().to_owned();
                match std::env::var("GITRUN_VERSION") {
                    Ok(binary_version) if binary_version.trim() != version => doctor_check(
                        "Installed version metadata",
                        Err(format!(
                            "GITRUN_VERSION reports {}, version.txt says {version}",
                            binary_version.trim()
                        )),
                        &mut failures,
                        &mut warnings,
                    ),
                    _ => doctor_check(
                        "Installed version metadata",
                        Ok(format!("version.txt is present ({version})")),
                        &mut failures,
                        &mut warnings,
                    ),
                }
            }
            Err(error) => doctor_check(
                "Installed version metadata",
                Err(format!("/usr/share/gitrun/version.txt: {error}")),
                &mut failures,
                &mut warnings,
            ),
        }
        doctor_check(
            "Control Manager group",
            doctor_command_status("/usr/bin/getent", &["group", "gitrun-control"])
                .map(|_| "gitrun-control exists".into())
                .map_err(|_| "missing gitrun-control group; run sudo gitrun fix-update".into()),
            &mut failures,
            &mut warnings,
        );
        let control_unit = [
            "/etc/systemd/system/gitrun-control.service",
            "/usr/lib/systemd/system/gitrun-control.service",
            "/lib/systemd/system/gitrun-control.service",
        ]
        .iter()
        .find(|path| Path::new(path).is_file());
        let systemd_dir = doctor_file_check("/etc/systemd/system", true)
            .map(|_| "/etc/systemd/system is present and root-owned".to_owned());
        doctor_check(
            "Systemd unit directory",
            systemd_dir,
            &mut failures,
            &mut warnings,
        );
        doctor_check(
            "Control Manager systemd unit",
            control_unit
                .map(|path| Ok((*path).to_owned()))
                .unwrap_or_else(|| {
                    Err("missing gitrun-control.service; run sudo gitrun fix-update".into())
                }),
            &mut failures,
            &mut warnings,
        );
        doctor_check(
            "Control Manager service",
            doctor_service_check("gitrun-control.service"),
            &mut failures,
            &mut warnings,
        );
        doctor_check(
            "GSR service",
            doctor_service_check("gitrun-gsr.service"),
            &mut failures,
            &mut warnings,
        );
        let socket_path = Path::new("/run/gitrun-control/manager.sock");
        match std::fs::symlink_metadata(socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() => doctor_check(
                "Control Manager socket",
                Ok(socket_path.display().to_string()),
                &mut failures,
                &mut warnings,
            ),
            Ok(_) => doctor_check(
                "Control Manager socket",
                Err(format!(
                    "{} exists but is not a socket",
                    socket_path.display()
                )),
                &mut failures,
                &mut warnings,
            ),
            Err(error) => doctor_check(
                "Control Manager socket",
                Err(format!("{}: {error}", socket_path.display())),
                &mut failures,
                &mut warnings,
            ),
        }
        let scheduler_unit = [
            "/etc/systemd/system/gitrun.service",
            "/usr/lib/systemd/system/gitrun.service",
            "/lib/systemd/system/gitrun.service",
        ]
        .iter()
        .find(|path| Path::new(path).is_file());
        let gsr_unit = [
            "/etc/systemd/system/gitrun-gsr.service",
            "/usr/lib/systemd/system/gitrun-gsr.service",
            "/lib/systemd/system/gitrun-gsr.service",
        ]
        .iter()
        .find(|path| Path::new(path).is_file());
        doctor_check(
            "GSR systemd unit",
            gsr_unit
                .map(|path| Ok((*path).to_owned()))
                .unwrap_or_else(|| {
                    Err("gitrun-gsr.service unit is missing; run sudo gitrun fix-update".into())
                }),
            &mut failures,
            &mut warnings,
        );
        doctor_check(
            "Scheduler systemd unit",
            scheduler_unit
                .map(|path| Ok((*path).to_owned()))
                .unwrap_or_else(|| Err("gitrun.service unit is missing".into())),
            &mut failures,
            &mut warnings,
        );
        if scheduler_unit.is_some() {
            doctor_check(
                "Scheduler service",
                doctor_service_check("gitrun.service"),
                &mut failures,
                &mut warnings,
            );
        } else {
            warnings += 1;
            println!("[WARN] Scheduler service: no systemd unit found; this may be a CLI-only installation");
        }
        match gitrun_setup::installation_state() {
            gitrun_setup::InstallationState::Complete => doctor_check(
                "Setup state",
                Ok("setup flag and system config are present".into()),
                &mut failures,
                &mut warnings,
            ),
            gitrun_setup::InstallationState::Incomplete => doctor_check(
                "Setup state",
                Err("installation is incomplete (config/setup flag mismatch)".into()),
                &mut failures,
                &mut warnings,
            ),
            gitrun_setup::InstallationState::FirstRun => doctor_check(
                "Setup state",
                Err("first-run setup has not completed".into()),
                &mut failures,
                &mut warnings,
            ),
        }
    } else {
        println!("[INFO] System service checks skipped: executable is not a recognized Linux system install");
    }

    if failures == 0 {
        println!("GitRun doctor: PASS (0 failures, {warnings} warning(s))");
        0
    } else {
        println!(
            "GitRun doctor: FAIL ({failures} failure(s), {warnings} warning(s)); see checks above"
        );
        1
    }
}
fn run_gtuu_command(
    repository_filter: Option<&str>,
    all_repositories: bool,
    rebuild: bool,
    force_replace: bool,
    check_only: bool,
    presenter: &mut presenter::CatPresenter,
) -> i32 {
    if !check_only && !rebuild && !force_replace {
        eprintln!("GitRun GTUU: choose --check, --rebuild, or --force; no operation was performed");
        return 2;
    }
    if repository_filter.is_none() && !all_repositories {
        eprintln!("GitRun GTUU: select --repo <owner/repository> or --all; no operation was performed");
        return 2;
    }

    let system_install = system_install_paths().is_some();

    // A system install reads /etc/gitrun/gitrun.env and operates on the root
    // Docker daemon. Delegate the whole operation rather than letting an
    // unprivileged invocation fail while loading protected configuration.
    if system_install && !running_as_root() {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("GitRun GTUU: FAIL — unable to resolve installed executable: {error}");
                return 1;
            }
        };
        presenter.prepare_for_external_process();
        let mut command = std::process::Command::new("/usr/bin/sudo");
        command.arg(executable).args(["--no-cat", "gtuu"]);
        if check_only {
            command.arg("--check");
        }
        if rebuild {
            command.arg("--rebuild");
        }
        if let Some(repo) = repository_filter {
            command.arg("--repo").arg(repo);
        } else {
            command.arg("--all");
        }
        if force_replace && !rebuild {
            command.arg("--force");
        }
        return match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!(
                    "GitRun GTUU: FAIL — unable to request administrator authorization: {error}"
                );
                1
            }
        };
    }

    if system_install {
        initialize_update_signing_environment(true);
        initialize_system_update_config_path(true);
    }

    if rebuild && !system_install {
        eprintln!("GitRun GTUU: FAIL — rebuilding the trusted local runner image requires the installed system command and administrator privileges");
        return 2;
    }

    match gitrun_scheduler::run_gtuu_with_options(
        repository_filter,
        rebuild,
        force_replace,
        check_only,
    ) {
        Ok(count) if check_only && count == 0 => {
            println!("GitRun GTUU check: PASS");
            0
        }
        Ok(count) if check_only => {
            eprintln!("GitRun GTUU check: FAIL ({count} issue(s))");
            1
        }
        Ok(count) => {
            println!("GitRun GTUU: updated {count} permanent runner(s)");
            0
        }
        Err(error) => {
            eprintln!("GitRun GTUU: FAIL — {error}");
            1
        }
    }
}

fn run_runner_only_update() -> Result<u32, Box<dyn std::error::Error>> {
    let system_install = system_install_paths().is_some();
    initialize_update_signing_environment(system_install);
    initialize_system_update_config_path(system_install);
    if system_install && update_config_path(true).is_none() {
        return Err(
            "trusted system config /etc/gitrun/gitrun.env is missing or unsafe; refusing runner update"
                .into(),
        );
    }

    let config = load_config()?;
    let _gtuu_lock = gitrun_scheduler::gtuu::GtuuLock::acquire(Path::new(&config.state_dir))?;
    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
    let manifest = latest_manifest(&repository)?;
    if let Some(image) = manifest
        .runner_image
        .as_ref()
        .filter(|image| image.reference.contains('/'))
    {
        let config_file = update_config_path(system_install).ok_or(
            "GitRun config path is unavailable; cannot safely pin the verified runner image",
        )?;
        update_runner_image(image)?;
        pin_runner_image(&config_file, image)?;
        println!(
            "Verified registry runner image pinned: {}@{}",
            image.reference, image.digest
        );
    } else if manifest.runner_image.is_none() {
        println!("Release manifest does not declare a registry runner image; checking the configured local image build fingerprint through GTUU.");
    }

    // GTUU refreshes local build inputs before comparing container image IDs,
    // then updates only eligible permanent runners and leaves busy runners intact.
    gitrun_scheduler::run_gtuu_once()
}

/// Repair the privileged control service even when the installed release is already current,
/// then run the normal signed updater only when GitHub advertises a newer release.
/// Never downgrade the installed executable to trick the version comparison.
fn run_fix_update(presenter: &mut presenter::CatPresenter) -> i32 {
    if system_install_paths().is_none() {
        eprintln!("GitRun fix-update: requires the trusted system installation at /usr/bin/gitrun or /usr/local/bin/gitrun.");
        return 2;
    }

    if !running_as_root() {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("GitRun fix-update: unable to resolve installed executable: {error}");
                return 1;
            }
        };
        presenter.prepare_for_external_process();
        let status = match std::process::Command::new("/usr/bin/sudo")
            .arg(executable)
            .arg("--no-cat")
            .arg("--fix-update-root")
            .status()
        {
            Ok(status) => status,
            Err(error) => {
                eprintln!("GitRun fix-update: unable to start privileged repair: {error}");
                return 1;
            }
        };
        return status.code().unwrap_or(1);
    }

    let (installed_binary, _) = match system_install_paths() {
        Some(paths) => paths,
        None => {
            eprintln!("GitRun fix-update: trusted system binary disappeared during repair");
            return 1;
        }
    };

    initialize_update_signing_environment(true);
    initialize_system_update_config_path(true);
    // Do not allow inherited environment variables to redirect a privileged
    // repair to a fork or a caller-selected repository.
    std::env::set_var("GITRUN_REPOSITORY", "Vider06/GitRun");

    let desktop_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|uid| *uid != 0);

    println!("GitRun fix-update: reconciling systemd units, control group and GitRun services");
    if let Err(error) =
        gitrun_setup::ensure_control_manager_service_with_binary(desktop_uid, &installed_binary)
    {
        eprintln!("GitRun fix-update: control service repair failed: {error}");
        return 1;
    }
    println!("GitRun fix-update: control service PASS");

    println!("GitRun fix-update: checking the latest official signed release");
    let manifest = match latest_manifest("Vider06/GitRun") {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("GitRun fix-update: unable to read the latest signed release: {error}");
            return 1;
        }
    };
    let current = current_version();
    let target = match target_triple() {
        Ok(target) => target,
        Err(error) => {
            eprintln!("GitRun fix-update: unsupported update target: {error}");
            return 1;
        }
    };

    match build_plan(&manifest, &current, &target, &dependency_snapshot()) {
        Ok(plan) => {
            println!(
                "GitRun fix-update: applying latest release {} -> {}",
                plan.current_version, plan.target_version
            );
            let code = run_update(None, false, false, presenter);
            if code != 0 {
                eprintln!("GitRun fix-update: latest-release update failed");
                return code;
            }
        }
        Err(UpdateError::NotNewer) => {
            println!(
                "GitRun fix-update: already on the latest release ({current}); no downgrade needed"
            );
        }
        Err(error) => {
            eprintln!("GitRun fix-update: release validation failed: {error}");
            return 1;
        }
    }

    if let Err(error) = verify_post_update_doctor(&installed_binary) {
        eprintln!("GitRun fix-update: final doctor verification failed: {error}");
        return 1;
    }
    println!("GitRun fix-update: PASS (systemd integration and doctor verified)");
    0
}

fn run_update_check(manifest_url: Option<&str>) -> i32 {
    // Keep the read-only path separate from the update transaction dispatcher:
    // this function never reaches dependency changes, file installation or
    // service restarts.
    let system_install = system_install_paths().is_some();
    initialize_update_signing_environment(system_install);
    initialize_system_update_config_path(system_install);
    let args: Vec<String> = match manifest_url {
        Some(url) => vec!["update".to_owned(), "--check".to_owned(), url.to_owned()],
        None => vec!["update".to_owned(), "--check".to_owned()],
    };
    match update_check_command(&args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun update check: FAIL — {error}");
            1
        }
    }
}

fn run_update(
    manifest_url: Option<&str>,
    only_containers: bool,
    force: bool,
    presenter: &mut presenter::CatPresenter,
) -> i32 {
    // Installed-system invocations must never inherit caller-selected update
    // keys, signature policy, or config paths. Direct-root and --only-containers
    // paths bypass sudo's normal signing-key hand-off, so initialize the fixed
    // trust anchors and service config before branching.
    let system_install = system_install_paths().is_some();
    if system_install || only_containers {
        initialize_update_signing_environment(system_install);
    }
    initialize_system_update_config_path(system_install);

    if only_containers {
        if system_install && !running_as_root() {
            let executable = match std::env::current_exe() {
                Ok(path) => path,
                Err(error) => {
                    eprintln!(
                        "GitRun GTUU: FAIL — unable to resolve installed executable: {error}"
                    );
                    return 1;
                }
            };
            presenter.prepare_for_external_process();
            return match std::process::Command::new("/usr/bin/sudo")
                .arg(executable)
                .args(["--no-cat", "update", "--only-containers"])
                .status()
            {
                Ok(status) => status.code().unwrap_or(1),
                Err(error) => {
                    eprintln!("GitRun GTUU: FAIL — unable to request administrator authorization: {error}");
                    1
                }
            };
        }
        presenter.transition(presenter::ValidationState::Running);
        return match run_runner_only_update() {
            Ok(count) => {
                println!("GitRun GTUU: updated {count} permanent runner(s)");
                if let Ok(config) = load_config() {
                    match refresh_json_setup_assets(&config, system_install) {
                        Ok((backups, library)) => println!("GitRun JSON library: {backups} missing backup(s) created; {library} Main Library file(s) added"),
                        Err(error) => eprintln!("GitRun GTUU: WARN — JSON library refresh will need retry: {error}"),
                    }
                }
                0
            }
            Err(error) => {
                eprintln!("GitRun GTUU: FAIL — {error}");
                1
            }
        };
    }

    if system_install_paths().is_some() && !running_as_root() {
        // Resolve and verify the release before invoking sudo. This avoids an
        // unnecessary password prompt when GitHub is unreachable or the
        // release cannot be applied on this host.
        println!(
            "GitRun update: verifying the signed release before requesting administrator access"
        );
        let repository =
            std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());
        let manifest = match manifest_url {
            Some(url) => fetch_manifest(url),
            None => latest_manifest(&repository),
        };
        let manifest = match manifest {
            Ok(manifest) => manifest,
            Err(error) => {
                eprintln!("GitRun update: FAIL — release preflight failed: {error}");
                return 1;
            }
        };
        let current = current_version();
        let target = match target_triple() {
            Ok(target) => target,
            Err(error) => {
                eprintln!("GitRun update: FAIL — unable to identify this platform: {error}");
                return 1;
            }
        };
        if let Err(error) = build_plan(&manifest, &current, &target, &dependency_snapshot()) {
            match error {
                UpdateError::NotNewer => {
                    println!(
                        "GitRun update: already up to date ({current}); no administrator access needed"
                    );
                    return 0;
                }
                error => {
                    eprintln!("GitRun update: FAIL — release preflight failed: {error}");
                    return 1;
                }
            }
        }

        // Pin the privileged retry to the already-verified release instead
        // of resolving a moving "latest" pointer for a second time.
        let resolved_manifest_url = match manifest_url {
            Some(url) => url.to_owned(),
            None => {
                match release_manifest_download_url(&repository, &manifest.version) {
                    Ok(url) => url,
                    Err(error) => {
                        eprintln!("GitRun update: FAIL — unable to resolve the signed release URL: {error}");
                        return 1;
                    }
                }
            }
        };
        match elevate_system_update(Some(&resolved_manifest_url), force, presenter) {
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
    match update_command(&args, presenter, force) {
        Ok(()) => {
            if let Ok(config) = load_config() {
                match refresh_json_setup_assets(&config, system_install) {
                    Ok((backups, library)) => println!("GitRun JSON library: {backups} missing backup(s) created; {library} Main Library file(s) added"),
                    Err(error) => eprintln!("GitRun update: WARN — JSON library refresh will need retry: {error}"),
                }
            }
            0
        }
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

fn run_recovery_gtuu(presenter: &mut presenter::CatPresenter) -> i32 {
    presenter.transition(presenter::ValidationState::Recovering);
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

#[cfg(target_os = "linux")]
fn reinstall_latest_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("verified GitRun reinstall requires administrator privileges".into());
    }
    if !cfg!(target_arch = "x86_64") {
        return Err("the official Debian reinstall currently targets Linux x86_64".into());
    }

    // Auto-reinstall is pinned to the official GitRun trust anchor.
    // Do not allow transient environment overrides to disable verification.
    std::env::set_var(
        "GITRUN_UPDATE_PUBLIC_KEY_HEX",
        DEFAULT_UPDATE_PUBLIC_KEY_HEX,
    );
    std::env::set_var("GITRUN_UPDATE_PUBLIC_KEY_ID", DEFAULT_UPDATE_PUBLIC_KEY_ID);
    std::env::set_var("GITRUN_UPDATE_SIGNATURE_REQUIRED", "true");
    let manifest = latest_manifest("Vider06/GitRun")?;
    let version = manifest.version.clone();
    let artifact = manifest.artifact_for("linux-x86_64-deb")?;
    let artifact_name = artifact.file.clone();
    let artifact_sha256 = artifact.sha256.clone();
    let url = format!(
        "https://github.com/Vider06/GitRun/releases/download/{}/{}",
        version, artifact_name
    );

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging =
        std::env::temp_dir().join(format!("gitrun-reinstall-{}-{now}", std::process::id()));
    std::fs::create_dir(&staging)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let package_file = staging.join(&artifact_name);
        println!("GitRun reinstall: verified latest release metadata for {version}");
        download_and_verify(&url, &artifact_sha256, &package_file)?;

        let package_name = std::process::Command::new("/usr/bin/dpkg-deb")
            .arg("-f")
            .arg(&package_file)
            .arg("Package")
            .output()?;
        if !package_name.status.success()
            || String::from_utf8_lossy(&package_name.stdout).trim() != "gitrun"
        {
            return Err("verified release artifact is not the GitRun Debian package".into());
        }
        let package_version = std::process::Command::new("/usr/bin/dpkg-deb")
            .arg("-f")
            .arg(&package_file)
            .arg("Version")
            .output()?;
        let expected_version = version.strip_prefix('v').unwrap_or(&version);
        if !package_version.status.success()
            || String::from_utf8_lossy(&package_version.stdout).trim() != expected_version
        {
            return Err("Debian package version does not match the signed release manifest".into());
        }

        let installed = std::process::Command::new("/usr/bin/dpkg-query")
            .args(["-W", "-f=${db:Status-Status}", "gitrun"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == "installed"
            });
        let mut apt = std::process::Command::new("/usr/bin/apt-get");
        apt.args(["install", "--yes", "--no-install-recommends"]);
        if installed {
            apt.arg("--reinstall");
        }
        let status = apt.arg(&package_file).status()?;
        if !status.success() {
            return Err(format!(
                "apt-get failed to install the verified GitRun package ({status})"
            )
            .into());
        }

        // systemd units use /usr/local/bin/gitrun, while the .deb owns
        // /usr/bin/gitrun. Atomically refresh the service entrypoint as well.
        install_debian_cli_as_service_binary()?;

        if Path::new("/etc/gitrun/gitrun.env").is_file() {
            let status = std::process::Command::new("/usr/local/bin/gitrun")
                .args(["--no-cat", "--reinstall-existing-root"])
                .status()?;
            if !status.success() {
                return Err(format!("latest release installed, but saved runtime configuration could not be rebuilt ({status})").into());
            }
        } else {
            println!("Latest GitRun package installed. No saved configuration was found; run gitrun setup --terminal to configure the host.");
        }
        println!("GitRun reinstall: PASS — installed verified release {version}");
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&staging);
    result
}

#[cfg(not(target_os = "linux"))]
fn reinstall_latest_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("automatic release reinstall currently targets Linux x86_64".into())
}

#[cfg(target_os = "linux")]
fn install_debian_cli_as_service_binary() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let source = Path::new("/usr/bin/gitrun");
    let destination = Path::new("/usr/local/bin/gitrun");
    if !source.is_file() {
        return Err("dpkg installed the package but /usr/bin/gitrun is missing".into());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary =
        destination.with_file_name(format!(".gitrun-reinstall-{}.tmp", std::process::id()));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        std::fs::copy(source, &temporary)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(target_os = "linux"))]
fn install_debian_cli_as_service_binary() -> Result<(), Box<dyn std::error::Error>> {
    Err("the official Debian service entrypoint is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn reinstall_existing_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("rebuilding the installed runtime requires administrator privileges".into());
    }
    let config_path = Path::new("/etc/gitrun/gitrun.env");
    if !config_path.is_file() {
        return Err("no saved GitRun configuration exists to rebuild".into());
    }
    let raw = std::fs::read_to_string(config_path)?;
    let mut values = std::collections::BTreeMap::new();
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            );
        }
    }

    let repositories = values
        .get("GITRUN_REPOSITORIES")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| values.get("GITRUN_DEFAULT_REPOSITORY"))
        .cloned()
        .ok_or("saved GitRun configuration has no repositories")?;
    let pat = values
        .get("GITHUB_TOKEN")
        .filter(|value| !value.trim().is_empty());
    let auth = if let Some(token) = pat {
        BootstrapAuth::Pat(token.clone())
    } else {
        BootstrapAuth::GitHubApp {
            app_id: values
                .get("GITRUN_GITHUB_APP_ID")
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or("saved configuration has neither a PAT nor a GitHub App ID")?,
            installation_id: values
                .get("GITRUN_GITHUB_APP_INSTALLATION_ID")
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or("saved configuration has no GitHub App installation ID")?,
            private_key_path: values
                .get("GITRUN_GITHUB_APP_PRIVATE_KEY_PATH")
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or("saved configuration has no GitHub App private key path")?,
        }
    };
    let owner_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    gitrun_setup::reinstall_linux_with_auth(
        auth,
        &repositories,
        Path::new("/usr/local/bin/gitrun"),
        owner_uid,
    )?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reinstall_existing_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("runtime reinstall currently targets Linux x86_64".into())
}

#[cfg(target_os = "linux")]
fn read_privileged_user_payload(
    input_path: &Path,
    requester_uid: u32,
    label: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;
    let before = std::fs::symlink_metadata(input_path)?;
    if before.file_type().is_symlink() || !before.is_file() || before.nlink() != 1 {
        return Err(format!(
            "{label} payload must be a regular file without symlinks or hard links"
        )
        .into());
    }
    if before.uid() != requester_uid {
        return Err(format!("{label} payload is not owned by the requesting user").into());
    }
    if before.permissions().mode() & 0o077 != 0 {
        return Err(format!("{label} payload permissions must be 0600 or stricter").into());
    }
    if before.len() > MAX_PAYLOAD_BYTES {
        return Err(format!("{label} payload exceeds the 1 MiB safety limit").into());
    }

    // Open the exact non-symlink inode we checked. In a sticky shared directory
    // such as /tmp, a same-UID process can still race a path-based read by
    // unlinking/replacing the payload after lstat; fstat must match the checked
    // inode, and subsequent parsing reads only from this already-open handle.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(input_path)?;
    let opened = file.metadata()?;
    if opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || opened.file_type().is_symlink()
        || !opened.is_file()
        || opened.nlink() != 1
        || opened.uid() != requester_uid
        || opened.permissions().mode() & 0o077 != 0
        || opened.len() > MAX_PAYLOAD_BYTES
    {
        return Err(format!("{label} payload changed during validation or is unsafe").into());
    }

    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(MAX_PAYLOAD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PAYLOAD_BYTES {
        return Err(format!("{label} payload exceeds the 1 MiB safety limit").into());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn trusted_root_config_for_state_access() -> Result<Config, Box<dyn std::error::Error>> {
    let path = trusted_system_config_path()
        .ok_or("trusted system GitRun configuration is missing or unsafe")?;
    let config = Config::from_env_file(path)?;
    config.validate()?;
    gitrun_setup::validate_system_state_directory(Path::new(&config.state_dir))?;
    Ok(config)
}

#[cfg(target_os = "linux")]
fn read_protected_state_file(
    path: &Path,
    label: &str,
    max_bytes: u64,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > max_bytes
    {
        return Err(format!(
            "{label} must be a root-owned regular file with safe permissions and bounded size"
        )
        .into());
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

#[cfg(target_os = "linux")]
fn read_vm_configs_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("reading system VM configuration requires administrator authorization".into());
    }
    let config = trusted_root_config_for_state_access()?;
    let state_dir = PathBuf::from(&config.state_dir);
    let path = state_dir.join("vm-configs.json");
    let _ = read_protected_state_file(&path, "VM configuration", 1024 * 1024)?;
    let configs = gitrun_scheduler::vm::load_vm_configs(&state_dir)?;
    std::println!("{}", serde_json::to_string(&configs)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_vm_configs_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("system VM configuration reading is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn save_vm_configs_root_command(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("saving system VM configuration requires administrator privileges".into());
    }
    let requester_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("VM configuration helper requires a verified requesting user UID")?;
    let bytes = read_privileged_user_payload(Path::new(input_path), requester_uid, "vm-configs")?;
    if bytes.len() > 1024 * 1024 {
        return Err("VM configuration payload exceeds the 1 MiB safety limit".into());
    }
    let configs: Vec<gitrun_scheduler::vm::VmConfig> = serde_json::from_slice(&bytes)?;
    let config = trusted_root_config_for_state_access()?;
    let state_dir = PathBuf::from(&config.state_dir);
    let path = state_dir.join("vm-configs.json");
    let _ = read_protected_state_file(&path, "VM configuration", 1024 * 1024)?;
    gitrun_scheduler::vm::save_vm_configs(&state_dir, &configs)?;
    // Re-open and validate the committed file's final properties as a postcondition.
    let _ = read_protected_state_file(&path, "saved VM configuration", 1024 * 1024)?
        .ok_or("saved VM configuration file unexpectedly disappeared")?;
    std::println!("GitRun VM configuration saved successfully.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn save_vm_configs_root_command(_input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("system VM configuration persistence is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn read_pending_hypervisor_decisions_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "reading system hypervisor decisions requires administrator authorization".into(),
        );
    }
    let config = trusted_root_config_for_state_access()?;
    let state_dir = PathBuf::from(&config.state_dir);
    let directory = state_dir.join("hypervisor-decisions");
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::println!("[]");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("hypervisor-decision directory is not a trusted root-owned directory".into());
    }

    let mut json_records = 0usize;
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        json_records += 1;
        if json_records > 1_000 {
            return Err(
                "hypervisor-decision directory exceeds the 1000-record safety limit".into(),
            );
        }
        let file = std::fs::symlink_metadata(entry.path())?;
        if file.file_type().is_symlink()
            || !file.is_file()
            || file.nlink() != 1
            || file.uid() != 0
            || file.permissions().mode() & 0o022 != 0
            || file.len() > 64 * 1024
        {
            return Err(
                "hypervisor-decision record is not a trusted root-owned regular file".into(),
            );
        }
    }

    let mut decisions = gitrun_core::hypervisor_decision::list_pending(&state_dir)?;
    // The dashboard needs a bounded response even if several VMs are pending.
    decisions.truncate(100);
    std::println!("{}", serde_json::to_string(&decisions)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_pending_hypervisor_decisions_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("system hypervisor decision reading is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn respond_hypervisor_decision_root_command(
    vm_name: &str,
    choice: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "responding to a system hypervisor decision requires administrator privileges".into(),
        );
    }
    // VM names are path-encoded by the decision protocol; accepting only the two
    // serialized enum spellings prevents the helper becoming a generic file writer.
    let choice = match choice {
        "retry_kvm" => gitrun_core::hypervisor_decision::DecisionChoice::RetryKvm,
        "use_virtual_box" => gitrun_core::hypervisor_decision::DecisionChoice::UseVirtualBox,
        _ => {
            return Err(
                "unsupported hypervisor decision; expected retry_kvm or use_virtual_box".into(),
            )
        }
    };
    let config = trusted_root_config_for_state_access()?;
    gitrun_core::hypervisor_decision::respond(Path::new(&config.state_dir), vm_name, choice)?;
    std::println!("GitRun hypervisor decision saved.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn respond_hypervisor_decision_root_command(
    _vm_name: &str,
    _choice: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("system hypervisor decision responses are supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn read_logic_rules_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "reading system Logic Containers rules requires administrator authorization".into(),
        );
    }
    let config = trusted_root_config_for_state_access()?;
    let state_dir = PathBuf::from(&config.state_dir);
    let path = state_dir.join("logic-containers.json");
    let rules = match read_protected_state_file(&path, "Logic Containers rules", 1024 * 1024)? {
        Some(raw) => {
            let rules: Vec<gitrun_scheduler::logic_containers::LogicRule> =
                serde_json::from_str(&raw)?;
            gitrun_scheduler::logic_containers::validate_rules(&rules)?;
            rules
        }
        None => Vec::new(),
    };
    std::println!("{}", serde_json::to_string(&rules)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_logic_rules_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("system Logic Containers rules are supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn save_logic_rules_root_command(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "saving system Logic Containers rules requires administrator authorization".into(),
        );
    }
    let requester_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("Logic Containers helper requires a verified requesting user UID")?;
    let bytes = read_privileged_user_payload(Path::new(input_path), requester_uid, "logic-rules")?;
    let rules: Vec<gitrun_scheduler::logic_containers::LogicRule> = serde_json::from_slice(&bytes)?;
    gitrun_scheduler::logic_containers::validate_rules(&rules)?;

    let config = trusted_root_config_for_state_access()?;
    let path = PathBuf::from(&config.state_dir).join("logic-containers.json");
    // Reject unsafe pre-existing files before atomic replacement. The parent
    // state directory has already been verified as root-owned and non-writable.
    let _ = read_protected_state_file(&path, "Logic Containers rules", 1024 * 1024)?;
    gitrun_scheduler::logic_containers::save_rules(&path, &rules)?;
    let _ = read_protected_state_file(&path, "Logic Containers rules", 1024 * 1024)?
        .ok_or("saved Logic Containers rules file unexpectedly disappeared")?;
    std::println!("GitRun Logic Containers rules saved successfully.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn save_logic_rules_root_command(_input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("system Logic Containers rules are supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn read_dock_requirements_root_command(repository: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "reading system GitDockRun requirements requires administrator authorization".into(),
        );
    }
    let config = trusted_root_config_for_state_access()?;
    if !config
        .repositories
        .iter()
        .any(|configured| configured == repository)
    {
        return Err(format!("repository is not configured in GitRun: {repository}").into());
    }

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let state_dir = PathBuf::from(&config.state_dir);
    let directory = state_dir.join("workflow-dock-requirements");
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::println!("[]");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(
            "GitDockRun requirements directory is not a trusted root-owned directory".into(),
        );
    }

    let path = directory.join(format!(
        "{}.json",
        gitrun_scheduler::docker::sanitize(repository, '_')
    ));
    let requirements =
        match read_protected_state_file(&path, "GitDockRun requirements", 1024 * 1024)? {
            Some(raw) => {
                let requests: Vec<gitrun_core::DockRequest> = serde_json::from_str(&raw)?;
                requests
            }
            None => Vec::new(),
        };
    std::println!("{}", serde_json::to_string(&requirements)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_dock_requirements_root_command(
    _repository: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("system GitDockRun requirements are supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn read_gsr_events_root_command(limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("reading system GSR events requires administrator authorization".into());
    }
    let config = trusted_root_config_for_state_access()?;
    let path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let events = gitrun_gsr::events::read_last(&path, limit.clamp(1, 500))?;
    // Machine-readable output is consumed by the dashboard after pkexec
    // authorizes this fixed, read-only operation. Never accept a caller path.
    std::println!("{}", serde_json::to_string(&events)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_gsr_events_root_command(_limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    Err("system GSR event reading is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn save_config_root_command(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("saving system settings requires administrator privileges".into());
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let requester_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("system config helper requires a verified requesting user UID")?;
    let bytes = read_privileged_user_payload(Path::new(input_path), requester_uid, "settings")?;
    let config: Config = serde_json::from_slice(&bytes)?;
    config.validate()?;
    let system_config = Path::new(TRUSTED_SYSTEM_CONFIG_FILE);
    let config_metadata = std::fs::symlink_metadata(system_config)?;
    let config_parent = system_config
        .parent()
        .ok_or("system config has no parent directory")?;
    let parent_metadata = std::fs::symlink_metadata(config_parent)?;
    if config_metadata.file_type().is_symlink()
        || !config_metadata.is_file()
        || config_metadata.uid() != 0
        || config_metadata.permissions().mode() & 0o022 != 0
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != 0
        || parent_metadata.permissions().mode() & 0o022 != 0
    {
        return Err("trusted system GitRun config path is missing or unsafe".into());
    }
    gitrun_setup::update_env_file(system_config, &config)?;
    gitrun_setup::secure_system_config_file(system_config, Some(requester_uid))?;
    std::fs::File::open(config_parent)?.sync_all()?;
    println!("GitRun settings saved successfully.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn save_config_root_command(_input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("system configuration persistence through the privileged helper is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn save_gitrun_settings_root_command(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("saving GitRun security settings requires administrator privileges".into());
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let requester_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("security-settings helper requires a verified requesting user UID")?;
    let bytes =
        read_privileged_user_payload(Path::new(input_path), requester_uid, "security-settings")?;
    let settings: GitRunSettings = serde_json::from_slice(&bytes)?;
    if settings.schema_version != gitrun_core::SETTINGS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported GitRun settings schema version {}",
            settings.schema_version
        )
        .into());
    }

    let config_path = Path::new(TRUSTED_SYSTEM_CONFIG_FILE);
    let config_metadata = std::fs::symlink_metadata(config_path)?;
    if config_metadata.file_type().is_symlink()
        || !config_metadata.is_file()
        || config_metadata.uid() != 0
        || config_metadata.permissions().mode() & 0o022 != 0
    {
        return Err("trusted system configuration is missing or unsafe".into());
    }
    let config = Config::from_env_file(config_path)?;
    config.validate()?;
    if settings.repositories.keys().any(|repo| {
        !config
            .repositories
            .iter()
            .any(|configured| configured == repo)
    }) {
        return Err(
            "security settings include a repository that is not configured in GitRun".into(),
        );
    }

    let state_dir = PathBuf::from(&config.state_dir);
    // Validate every ancestor before writing privileged policy state; an
    // untrusted ancestor could otherwise rename a trusted-looking child dir.
    gitrun_setup::validate_system_state_directory(&state_dir)?;

    let destination = GitRunSettings::path_for_state_dir(&state_dir);
    if let Ok(metadata) = std::fs::symlink_metadata(&destination) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("GitRun settings destination must be a regular non-symlink file".into());
        }
    }

    settings.save(&destination)?;
    // The root scheduler treats this file as authorization policy. Keep it
    // root-owned and read-only to the desktop user's primary group; the user
    // must use this authenticated helper rather than editing the policy JSON.
    gitrun_setup::secure_system_settings_file(&destination, Some(requester_uid))?;
    std::fs::File::open(&state_dir)?.sync_all()?;
    println!("GitRun security settings saved successfully.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn save_gitrun_settings_root_command(_input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("system security-settings persistence through the privileged helper is Linux-only".into())
}

#[cfg(target_os = "linux")]
fn promote_staged_kitty_settings(
    staged: &Path,
    destination: &Path,
    expected_owner: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if staged.parent() != destination.parent() {
        return Err("staged Kitty settings must share the destination directory".into());
    }
    let staged_metadata = std::fs::symlink_metadata(staged)?;
    if staged_metadata.file_type().is_symlink()
        || !staged_metadata.is_file()
        || staged_metadata.uid() != expected_owner
        || staged_metadata.nlink() != 1
        || staged_metadata.permissions().mode() & 0o777 != 0o640
    {
        return Err("staged Kitty settings failed its ownership/permission postcondition".into());
    }

    match std::fs::symlink_metadata(destination) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.uid() == expected_owner
                && metadata.nlink() == 1
                && metadata.permissions().mode() & 0o022 == 0 => {}
        Ok(_) => {
            return Err(
                "existing Kitty settings path has unsafe type, owner, or permissions".into(),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let parent = destination
        .parent()
        .ok_or("Kitty settings destination has no parent directory")?;
    std::fs::rename(staged, destination)?;
    std::fs::File::open(parent)?.sync_all()?;

    let final_metadata = std::fs::symlink_metadata(destination)?;
    if final_metadata.file_type().is_symlink()
        || !final_metadata.is_file()
        || final_metadata.uid() != expected_owner
        || final_metadata.gid() != staged_metadata.gid()
        || final_metadata.nlink() != 1
        || final_metadata.permissions().mode() & 0o777 != 0o640
    {
        return Err("saved Kitty settings failed final ownership/permission verification".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_gitrun_settings_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "reading system GitRun security settings requires administrator authorization".into(),
        );
    }
    let config = trusted_root_config_for_state_access()?;
    let path = gitrun_core::GitRunSettings::path_for_state_dir(Path::new(&config.state_dir));
    // Validate the trusted settings file before decoding; never follow a symlink,
    // accept another owner's file, or expose an unbounded policy payload.
    let _ = read_protected_state_file(&path, "GitRun security settings", 1024 * 1024)?;
    let settings = gitrun_core::GitRunSettings::load_or_default(&path)?;
    std::println!("{}", serde_json::to_string(&settings)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_gitrun_settings_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("system GitRun security settings reading is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn read_kitty_dashboard_root_command() -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err(
            "reading the system Kitty dashboard requires administrator authorization".into(),
        );
    }
    let config = trusted_root_config_for_state_access()?;
    let state_dir = PathBuf::from(&config.state_dir);
    let settings = gitrun_smart_scheduler::load_settings_for_dashboard(&state_dir)?;
    let (status, status_error) = match gitrun_smart_scheduler::load_dashboard_status(&state_dir) {
        Ok(status) => (status, None),
        Err(error) => (
            gitrun_smart_scheduler::KittyDashboardStatus {
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
    std::println!(
        "{}",
        serde_json::json!({
            "settings": settings,
            "status": status,
            "status_error": status_error
        })
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn read_kitty_dashboard_root_command() -> Result<(), Box<dyn std::error::Error>> {
    Err("system Kitty dashboard reading is supported on Linux only".into())
}

#[cfg(target_os = "linux")]
fn save_kitty_settings_root_command(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !running_as_root() {
        return Err("saving Kitty settings requires administrator privileges".into());
    }
    let requester_uid = std::env::var("PKEXEC_UID")
        .or_else(|_| std::env::var("SUDO_UID"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("Kitty settings helper requires a verified requesting user UID")?;
    let bytes =
        read_privileged_user_payload(Path::new(input_path), requester_uid, "kitty-settings")?;
    let settings: KittySettings = serde_json::from_slice(&bytes)?;
    settings.validate()?;

    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let config_path = Path::new(TRUSTED_SYSTEM_CONFIG_FILE);
    let config_metadata = std::fs::symlink_metadata(config_path)?;
    let config_parent = config_path
        .parent()
        .ok_or("trusted system config has no parent directory")?;
    let parent_metadata = std::fs::symlink_metadata(config_parent)?;
    if config_metadata.file_type().is_symlink()
        || !config_metadata.is_file()
        || config_metadata.uid() != 0
        || config_metadata.permissions().mode() & 0o022 != 0
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != 0
        || parent_metadata.permissions().mode() & 0o022 != 0
    {
        return Err("trusted system GitRun configuration is missing or unsafe".into());
    }

    let config = Config::from_env_file(config_path)?;
    config.validate()?;
    let state_dir = PathBuf::from(&config.state_dir);
    gitrun_setup::validate_system_state_directory(&state_dir)?;
    let destination = gitrun_smart_scheduler::KittyRuntime::settings_path(&state_dir);
    let parent = destination
        .parent()
        .ok_or("Kitty settings path has no parent directory")?;
    if parent != config_parent {
        return Err(
            "Kitty settings path does not resolve to the trusted GitRun configuration directory"
                .into(),
        );
    }

    // Stage in the same trusted directory but outside the active settings path.
    // The scheduler can never observe the new values until ownership and
    // group-readable permissions have been verified successfully.
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staged = parent.join(format!(
        ".kitty-settings-pending-{}-{unique}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&staged)?;
        file.write_all(&serde_json::to_vec(&settings)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

        // This performs the existing root:desktop-primary-group chown + 0640
        // update and validates the postconditions before the active file moves.
        gitrun_setup::secure_system_settings_file(&staged, Some(requester_uid))?;
        promote_staged_kitty_settings(&staged, &destination, 0)?;
        Ok(())
    })();

    if result.is_err() {
        // This only removes the pending file; a previously active settings file
        // remains intact unless the staged file passed all checks and was renamed.
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    println!("Kitty settings saved successfully.");
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn save_kitty_settings_root_command(_input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("system Kitty settings persistence through the privileged helper is Linux-only".into())
}

#[cfg(all(test, target_os = "linux"))]
mod kitty_settings_promotion_tests {
    use super::promote_staged_kitty_settings;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after UNIX epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gitrun-kitty-promotion-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("test temp directory should be created");
        dir
    }

    fn set_mode(path: &std::path::Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .expect("test file permissions should be set");
    }

    #[test]
    fn staged_settings_atomically_replace_a_safe_existing_file() {
        let dir = temp_dir("replace");
        let destination = dir.join("kitty.json");
        let staged = dir.join(".kitty-settings-pending.tmp");
        fs::write(&destination, b"old-settings").unwrap();
        fs::write(&staged, b"new-settings").unwrap();
        set_mode(&destination, 0o640);
        set_mode(&staged, 0o640);
        let owner = fs::metadata(&staged).unwrap().uid();

        promote_staged_kitty_settings(&staged, &destination, owner).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new-settings");
        assert!(!staged.exists());
        let metadata = fs::symlink_metadata(&destination).unwrap();
        assert_eq!(metadata.uid(), owner);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_settings_refuse_a_symlink_destination_and_preserve_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink-destination");
        let outside = dir.join("outside.json");
        let destination = dir.join("kitty.json");
        let staged = dir.join(".kitty-settings-pending.tmp");
        fs::write(&outside, b"outside-settings").unwrap();
        fs::write(&staged, b"new-settings").unwrap();
        set_mode(&staged, 0o640);
        symlink(&outside, &destination).unwrap();
        let owner = fs::metadata(&staged).unwrap().uid();

        assert!(promote_staged_kitty_settings(&staged, &destination, owner).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside-settings");
        assert!(fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(staged.is_file());

        fs::remove_dir_all(dir).unwrap();
    }
}

fn run_reinstall_root(token_path: &str) -> i32 {
    match reinstall_root_command(token_path) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun reinstall: FAIL — {error}");
            1
        }
    }
}

fn run_uninstall_root(package_hook: bool) -> i32 {
    let result = if package_hook {
        gitrun_setup::uninstall_linux_package_hook()
    } else {
        gitrun_setup::uninstall_linux()
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun uninstall: FAIL — {error}");
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

fn run_rollback(backup_path: &str, presenter: &mut presenter::CatPresenter) -> i32 {
    match rollback_command(backup_path, presenter) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun rollback: FAIL — {error}");
            1
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod privileged_payload_tests {
    use super::read_privileged_user_payload;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gitrun-privileged-payload-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn privileged_payload_reads_only_private_regular_user_owned_files() {
        let path = temp_path("valid");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        use std::io::Write;
        file.write_all(br#"{"schema_version":1}"#).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let uid = std::fs::metadata(&path).unwrap().uid();
        let payload = read_privileged_user_payload(&path, uid, "test").unwrap();
        assert_eq!(payload.as_slice(), br#"{"schema_version":1}"#.as_slice());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn privileged_payload_rejects_symlinks_hardlinks_and_broad_permissions() {
        let original = temp_path("original");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&original)
            .unwrap();
        use std::io::Write;
        file.write_all(b"payload").unwrap();
        drop(file);
        let uid = std::fs::metadata(&original).unwrap().uid();

        let link = temp_path("symlink");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        assert!(read_privileged_user_payload(&link, uid, "test").is_err());
        std::fs::remove_file(&link).unwrap();

        let hardlink = temp_path("hardlink");
        std::fs::hard_link(&original, &hardlink).unwrap();
        assert!(read_privileged_user_payload(&original, uid, "test").is_err());
        std::fs::remove_file(&hardlink).unwrap();

        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_privileged_user_payload(&original, uid, "test").is_err());
        std::fs::remove_file(&original).unwrap();
    }

    #[test]
    fn privileged_payload_enforces_the_size_limit() {
        let path = temp_path("oversized");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert!(read_privileged_user_payload(&path, uid, "test").is_err());
        std::fs::remove_file(&path).unwrap();
    }
}

#[cfg(test)]
mod internal_lifecycle_command_tests {
    use super::{
        select_update_config_path, update_signing_environment, update_signing_environment_changes,
        Cli, Command, DEFAULT_UPDATE_PUBLIC_KEY_HEX, DEFAULT_UPDATE_PUBLIC_KEY_ID,
        TRUSTED_SYSTEM_CONFIG_FILE,
    };
    use clap::Parser;

    #[test]
    fn terminal_setup_disables_live_mascot_during_prompts() {
        assert!(super::is_interactive_terminal_setup(&Command::Setup {
            terminal: true
        }));
        assert!(!super::is_interactive_terminal_setup(&Command::Setup {
            terminal: false
        }));
        assert!(!super::is_interactive_terminal_setup(&Command::Doctor));
    }

    #[test]
    fn fix_update_commands_are_registered() {
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "fix-update"]).map(|cli| cli.command),
            Ok(Some(Command::FixUpdate))
        ));
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "--no-cat", "--fix-update-root"]).map(|cli| cli.command),
            Ok(Some(Command::FixUpdateRoot))
        ));
    }

    #[test]
    fn system_update_signing_policy_overrides_every_preexisting_value() {
        let changes = update_signing_environment_changes(true, |_| true);
        assert_eq!(changes, update_signing_environment());
        assert!(changes.iter().any(|(key, value)| {
            key == "GITRUN_UPDATE_PUBLIC_KEY_HEX" && value == DEFAULT_UPDATE_PUBLIC_KEY_HEX
        }));
        assert!(changes.iter().any(|(key, value)| {
            key == "GITRUN_UPDATE_PUBLIC_KEY_ID" && value == DEFAULT_UPDATE_PUBLIC_KEY_ID
        }));
        assert!(changes
            .iter()
            .any(|(key, value)| { key == "GITRUN_UPDATE_SIGNATURE_REQUIRED" && value == "true" }));
    }

    #[test]
    fn non_system_update_signing_policy_preserves_explicit_custom_values() {
        assert!(update_signing_environment_changes(false, |_| true).is_empty());

        let missing_key_only =
            update_signing_environment_changes(false, |key| key != "GITRUN_UPDATE_PUBLIC_KEY_HEX");
        assert_eq!(
            missing_key_only,
            vec![(
                "GITRUN_UPDATE_PUBLIC_KEY_HEX".to_owned(),
                DEFAULT_UPDATE_PUBLIC_KEY_HEX.to_owned()
            )]
        );
    }

    #[test]
    fn update_check_flag_is_parsed_as_read_only_mode() {
        let cli = Cli::try_parse_from(["gitrun", "update", "--check"])
            .expect("update --check should be a valid read-only request");
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                check: true,
                only_containers: false,
                force: false,
                ..
            })
        ));
    }

    #[test]
    fn system_update_config_path_ignores_environment_selected_paths() {
        let result = select_update_config_path(
            true,
            Some(std::path::PathBuf::from("/tmp/attacker-selected.env")),
        );
        assert_eq!(
            result,
            Some(std::path::PathBuf::from(TRUSTED_SYSTEM_CONFIG_FILE))
        );
    }

    #[test]
    fn developer_update_config_path_keeps_explicit_custom_path() {
        let custom = std::path::PathBuf::from("/tmp/developer-selected.env");
        assert_eq!(
            select_update_config_path(false, Some(custom.clone())),
            Some(custom)
        );
    }

    fn parse_internal_command(arguments: &[&str]) -> Command {
        let cli = Cli::try_parse_from(arguments.iter().copied())
            .unwrap_or_else(|error| panic!("failed to parse {:?}: {error}", arguments));
        assert!(
            cli.no_cat,
            "--no-cat must remain available before internal commands"
        );
        cli.command.expect("internal command must be selected")
    }

    #[test]
    fn manage_is_public_and_cat_remains_a_separate_command() {
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "manage"]).unwrap().command,
            Some(Command::Manage)
        ));
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "cat"]).unwrap().command,
            Some(Command::Cat)
        ));
    }

    #[test]
    fn update_force_skips_only_the_post_update_doctor_gate_option() {
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "update", "--force"]).map(|cli| cli.command),
            Ok(Some(Command::Update {
                force: true,
                check: false,
                only_containers: false,
                ..
            }))
        ));
        assert!(Cli::try_parse_from(["gitrun", "update", "--force", "--check"]).is_err());
        assert!(Cli::try_parse_from(["gitrun", "update", "--force", "--only-containers"]).is_err());
    }

    #[test]
    fn json_recovery_internal_action_is_allowlisted_by_clap() {
        assert!(matches!(
            Cli::try_parse_from(["gitrun", "--no-cat", "--json-recovery-root", "repair"])
                .map(|cli| cli.command),
            Ok(Some(Command::JsonRecoveryRoot { action })) if action == "repair"
        ));
        assert!(Cli::try_parse_from([
            "gitrun", "--no-cat", "--json-recovery-root", "../../etc/shadow"
        ]).is_ok(), "the CLI parser does not interpret the fixed action string; the privileged handler rejects unknown actions");
    }

    #[test]
    fn no_cat_is_accepted_before_and_after_the_doctor_subcommand() {
        let before = Cli::try_parse_from(["gitrun", "--no-cat", "doctor"]).unwrap();
        assert!(before.no_cat);
        assert!(matches!(before.command, Some(Command::Doctor)));

        // Backward compatibility: an older updater can remain running while
        // it replaces the installed executable, then invoke the new binary
        // using the historical `doctor --no-cat` argument order.
        let after = Cli::try_parse_from(["gitrun", "doctor", "--no-cat"]).unwrap();
        assert!(after.no_cat);
        assert!(matches!(after.command, Some(Command::Doctor)));
    }

    #[test]
    fn no_cat_override_is_detected_before_clap_parsing() {
        assert!(super::no_cat_override_requested([
            std::ffi::OsString::from("--no-cat"),
            std::ffi::OsString::from("doctor"),
        ]));
        // Clap global flags may appear after the subcommand too; mascot
        // suppression must be recognized before parsing in either position.
        assert!(super::no_cat_override_requested([
            std::ffi::OsString::from("doctor"),
            std::ffi::OsString::from("--no-cat"),
        ]));
        assert!(!super::no_cat_override_requested([
            std::ffi::OsString::from("doctor"),
        ]));
    }

    #[test]
    fn fast_stop_requires_explicit_confirmation() {
        assert!(Cli::try_parse_from(["gitrun", "fast-stop", "owner/repo", "123"]).is_err());
        assert!(Cli::try_parse_from(["gitrun", "fast-stop", "owner/repo", "123", "--yes"]).is_ok());
    }

    #[test]
    fn privileged_lifecycle_entry_points_parse_as_invoked_by_helpers() {
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--uninstall-root"]),
            Command::UninstallRoot
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--uninstall-package-hook"]),
            Command::UninstallPackageHook
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--reinstall-latest-root"]),
            Command::ReinstallLatestRoot
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--reinstall-existing-root"]),
            Command::ReinstallExistingRoot
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--save-config-root",
                "/tmp/private-settings.json"
            ]),
            Command::SaveConfigRoot { input_path }
                if input_path == "/tmp/private-settings.json"
        ));

        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--read-gsr-events-root", "30"]),
            Command::ReadGsrEventsRoot { limit: 30 }
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--read-logic-rules-root"]),
            Command::ReadLogicRulesRoot
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--read-vm-configs-root"]),
            Command::ReadVmConfigsRoot
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--save-vm-configs-root",
                "/tmp/private-vm-configs.json"
            ]),
            Command::SaveVmConfigsRoot { input_path }
                if input_path == "/tmp/private-vm-configs.json"
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--read-pending-hypervisor-decisions-root"
            ]),
            Command::ReadPendingHypervisorDecisionsRoot
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--respond-hypervisor-decision-root",
                "--",
                "-win-runner",
                "use_virtual_box"
            ]),
            Command::RespondHypervisorDecisionRoot { vm_name, choice }
                if vm_name == "-win-runner" && choice == "use_virtual_box"
        ));

        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--save-logic-rules-root",
                "/tmp/private-logic-rules.json"
            ]),
            Command::SaveLogicRulesRoot { input_path }
                if input_path == "/tmp/private-logic-rules.json"
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--read-dock-requirements-root",
                "owner/repo"
            ]),
            Command::ReadDockRequirementsRoot { repository }
                if repository == "owner/repo"
        ));

        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--save-gitrun-settings-root",
                "/tmp/private-security-settings.json"
            ]),
            Command::SaveGitRunSettingsRoot { input_path }
                if input_path == "/tmp/private-security-settings.json"
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--save-kitty-settings-root",
                "/tmp/private-kitty-settings.json"
            ]),
            Command::SaveKittySettingsRoot { input_path }
                if input_path == "/tmp/private-kitty-settings.json"
        ));

        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--read-gitrun-settings-root"]),
            Command::ReadGitRunSettingsRoot
        ));
        assert!(matches!(
            parse_internal_command(&["gitrun", "--no-cat", "--read-kitty-dashboard-root"]),
            Command::ReadKittyDashboardRoot
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--install-root",
                "/tmp/private-token.json"
            ]),
            Command::InstallRoot { token_path }
                if token_path == "/tmp/private-token.json"
        ));
        assert!(matches!(
            parse_internal_command(&[
                "gitrun",
                "--no-cat",
                "--reinstall-root",
                "/tmp/private-token.json"
            ]),
            Command::ReinstallRoot { token_path }
                if token_path == "/tmp/private-token.json"
        ));
    }
}

#[cfg(test)]
mod doctor_json_validation_tests {
    use super::doctor_json_file_check;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gitrun-doctor-json-{label}-{}-{nonce}.json",
            std::process::id()
        ))
    }

    #[test]
    fn doctor_validates_each_json_file_not_just_the_directory() {
        let valid = path("valid");
        std::fs::write(&valid, r#"{"schema_version":1,"enabled":false}"#).unwrap();
        assert!(doctor_json_file_check(&valid, false).is_ok());
        std::fs::remove_file(valid).unwrap();

        let invalid = path("invalid");
        std::fs::write(&invalid, r#"{"schema_version":1,}"#).unwrap();
        assert!(doctor_json_file_check(&invalid, false)
            .unwrap_err()
            .contains("invalid JSON"));
        std::fs::remove_file(invalid).unwrap();
    }

    #[test]
    fn doctor_rejects_scalar_json_roots() {
        let scalar = path("scalar");
        std::fs::write(&scalar, "true").unwrap();
        assert!(doctor_json_file_check(&scalar, false)
            .unwrap_err()
            .contains("scalar JSON root"));
        std::fs::remove_file(scalar).unwrap();
    }

    #[test]
    fn doctor_rejects_json_files_that_are_too_large() {
        let large = path("large");
        std::fs::write(&large, vec![b' '; 16 * 1024 * 1024 + 1]).unwrap();
        assert!(doctor_json_file_check(&large, false)
            .unwrap_err()
            .contains("16 MiB JSON safety limit"));
        std::fs::remove_file(large).unwrap();
    }
}
