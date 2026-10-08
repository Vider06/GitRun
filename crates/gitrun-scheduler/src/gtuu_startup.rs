//! Full GTUU orchestration for GitRun startup.

use crate::{
    build_github_client, load_config, resolve_docker_socket_gid, vault_env_for_repo, GtuuConfig,
};
use gitrun_core::{Config, GitRunSettings};
use gitrun_updater::{self, UpdateError};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct GtuuStartupReport {
    pub current_version: String,
    pub target_version: Option<String>,
    pub gitrun_updated: bool,
    pub gitrun_update_error: Option<String>,
    pub runner_image_error: Option<String>,
    pub containers_updated: u32,
    pub containers_error: Option<String>,
}

pub fn run_gtuu_startup_once() -> Result<GtuuStartupReport, Box<dyn std::error::Error>> {
    let config = load_config()?;
    if config.repositories.is_empty() {
        return Err("GTUU requires at least one configured repository".into());
    }

    let _lock = crate::gtuu::GtuuLock::acquire(&PathBuf::from(&config.state_dir))?;
    let current_version = current_gitrun_version()?;
    let mut report = GtuuStartupReport {
        current_version: current_version.clone(),
        target_version: None,
        gitrun_updated: false,
        gitrun_update_error: None,
        runner_image_error: None,
        containers_updated: 0,
        containers_error: None,
    };

    let repository = std::env::var("GITRUN_REPOSITORY").unwrap_or_else(|_| "Vider06/GitRun".into());

    let manifest = match gitrun_updater::latest_manifest(&repository) {
        Ok(manifest) => {
            report.target_version = Some(manifest.version.clone());
            Some(manifest)
        }
        Err(error) => {
            report.gitrun_update_error = Some(error.to_string());
            None
        }
    };

    let mut runner_image = config.runner_image.clone();
    let mut runner_image_ready = true;

    if let Some(manifest) = manifest.as_ref() {
        match update_gitrun_from_manifest(&config, manifest, &current_version) {
            Ok(updated) => {
                report.gitrun_updated = updated;
                if updated {
                    report.current_version = manifest.version.clone();
                }
            }
            Err(error) => report.gitrun_update_error = Some(error.to_string()),
        }

        if let Some(image) = &manifest.runner_image {
            match gitrun_updater::update_runner_image(image) {
                Ok(()) => {
                    match configured_config_file() {
                        Some(path) => match gitrun_updater::pin_runner_image(path, image) {
                            Ok(()) => runner_image = image.reference.clone(),
                            Err(error) => {
                                report.runner_image_error = Some(error.to_string());
                                runner_image_ready = false;
                            }
                        },
                        None => {
                            report.runner_image_error =
                            Some("runner image updated but no GitRun config file is available to pin it".into());
                            runner_image_ready = false;
                        }
                    }
                }
                Err(error) => {
                    report.runner_image_error = Some(error.to_string());
                    runner_image_ready = false;
                }
            }
        }
    }

    if !runner_image_ready {
        report.containers_error =
            Some("permanent containers were not reconciled because the runner image state is inconsistent".into());
        return Ok(report);
    }

    let client = match build_github_client(&config) {
        Ok(client) => client,
        Err(error) => {
            report.containers_error = Some(error.to_string());
            return Ok(report);
        }
    };

    let settings =
        GitRunSettings::load_or_default(GitRunSettings::path_for_state_dir(&config.state_dir))?;
    let socket_enabled_for_repo = |repo: &str| {
        settings
            .effective_for_repository(repo)
            .docker
            .direct_socket_enabled
    };
    let docker_socket_gid = if config
        .repositories
        .iter()
        .any(|repo| socket_enabled_for_repo(repo))
    {
        match resolve_docker_socket_gid() {
            Ok(gid) => gid,
            Err(error) => {
                report.containers_error = Some(error.to_string());
                return Ok(report);
            }
        }
    } else {
        String::new()
    };

    let gsr_policy_env = super::gsr_policy_env(&config);
    let gtuu_config = GtuuConfig {
        image: &runner_image,
        repositories: &config.repositories,
        runner_labels: &config.runner_labels,
        ephemeral: config.ephemeral,
        disable_update: config.runner_disable_update,
        cpus: &config.container_cpus,
        memory: &config.container_memory,
        pids_limit: &config.container_pids_limit,
        shared_cache_volume: &config.shared_cache_volume,
        cache_scope: &config.shared_cache_scope,
        network: &config.runner_network,
        seccomp_profile: &config.runner_seccomp_profile,
        apparmor_profile: &config.runner_apparmor_profile,
        docker_socket_gid: &docker_socket_gid,
        runner_home_size: &config.runner_home_size,
        runner_home_backend: crate::docker::RunnerHomeBackend::from_config_str(
            &config.runner_home_backend,
        ),
        runner_rootfs_read_only: config.runner_rootfs_read_only,
        runner_windows_hyperv_isolation: config.runner_windows_hyperv_isolation,
        secret_env_for_repo: &|repo: &str| vault_env_for_repo(&config, repo),
        online_wait_timeout: Duration::from_secs(120),
        docker_socket_hardening: config.gsr_docker_socket_hardening,
        gsr_policy_env: &gsr_policy_env,
        docker_socket_enabled_for_repo: &socket_enabled_for_repo,
    };

    match crate::gtuu::update_permanent_containers(&client, &gtuu_config) {
        Ok(count) => report.containers_updated = count,
        Err(error) => report.containers_error = Some(error.to_string()),
    }

    Ok(report)
}

fn update_gitrun_from_manifest(
    config: &Config,
    manifest: &gitrun_updater::ReleaseManifest,
    current_version: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !cfg!(target_os = "linux") {
        return Err("system GitRun self-update currently targets Linux".into());
    }

    let target = target_triple_for_gitrun()?;
    let plan = match gitrun_updater::build_plan(manifest, current_version, &target, &[]) {
        Ok(plan) => plan,
        Err(UpdateError::NotNewer) => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    let work_root = std::env::var("GITRUN_UPDATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&config.state_dir).join("update"));
    std::fs::create_dir_all(&work_root)?;
    let archive = work_root.join(&plan.artifact);
    let artifact = manifest.artifact_for(&target)?;
    gitrun_updater::download_and_verify(&plan.artifact_url, &artifact.sha256, &archive)?;

    let gitrun_binary = PathBuf::from(
        std::env::var("GITRUN_BINARY_PATH").unwrap_or_else(|_| "/usr/local/bin/gitrun".into()),
    );
    let dashboard_binary = PathBuf::from(
        std::env::var("GITRUN_DASHBOARD_BINARY")
            .unwrap_or_else(|_| "/usr/bin/gitrun-dashboard-tauri".into()),
    );
    let recovery_binary = PathBuf::from(
        std::env::var("GITRUN_RECOVERY_BINARY")
            .unwrap_or_else(|_| "/usr/local/bin/gitrun-recovery".into()),
    );
    let version_file = PathBuf::from(
        std::env::var("GITRUN_VERSION_FILE")
            .unwrap_or_else(|_| "/usr/share/gitrun/version.txt".into()),
    );
    let backup_root = PathBuf::from(std::env::var("GITRUN_BACKUP_DIR").unwrap_or_else(|_| {
        PathBuf::from(&config.state_dir)
            .join("backups")
            .display()
            .to_string()
    }));

    let artifacts = [
        gitrun_updater::InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: gitrun_binary.clone(),
            required: true,
        },
        gitrun_updater::InstalledArtifact {
            archive_name: "gitrun-dashboard-tauri".into(),
            destination: dashboard_binary,
            required: false,
        },
        gitrun_updater::InstalledArtifact {
            archive_name: "gitrun-recovery".into(),
            destination: recovery_binary,
            required: false,
        },
    ];

    gitrun_updater::apply_installed_update(
        &archive,
        &target,
        &manifest.version,
        &artifacts,
        &version_file,
        &backup_root,
        &gitrun_binary,
        configured_config_file().as_deref(),
    )?;

    Ok(true)
}

fn configured_config_file() -> Option<PathBuf> {
    std::env::var("GITRUN_CONFIG_FILE")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let path = PathBuf::from("/etc/gitrun/gitrun.env");
            path.is_file().then_some(path)
        })
}

fn current_gitrun_version() -> Result<String, Box<dyn std::error::Error>> {
    if let Some(value) = std::env::var("GITRUN_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(value);
    }

    for path in ["/usr/share/gitrun/version.txt", "version.txt"] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_owned());
            }
        }
    }

    Err("unable to determine the installed GitRun version".into())
}

fn target_triple_for_gitrun() -> Result<String, Box<dyn std::error::Error>> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".into()),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu".into()),
        (os, arch) => Err(format!("unsupported GitRun self-update target: {os}-{arch}").into()),
    }
}
