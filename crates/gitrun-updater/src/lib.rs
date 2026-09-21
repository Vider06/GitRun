use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseArtifact {
    pub target: String,
    pub file: String,
    pub sha256: String,
    #[serde(default)]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyRequirement {
    pub name: String,
    pub minimum_version: String,
    #[serde(default)]
    pub recommended_version: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub installation_method: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerImage {
    pub reference: String,
    pub digest: String,
    pub minimum_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub name: String,
    pub version: String,
    pub git_commit: String,
    pub artifacts: Vec<ReleaseArtifact>,
    #[serde(default)]
    pub dependencies: Vec<DependencyRequirement>,
    #[serde(default)]
    pub runner_image: Option<RunnerImage>,
    #[serde(default)]
    pub repository: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyStatus {
    pub name: String,
    pub installed_version: Option<String>,
    pub minimum_version: String,
    pub compatible: bool,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdatePlan {
    pub current_version: String,
    pub target_version: String,
    pub target: String,
    pub artifact: String,
    pub artifact_url: String,
    pub dependencies: Vec<DependencyStatus>,
    pub runner_image: Option<RunnerImage>,
}

#[derive(Debug, Clone)]
pub struct UpdatePaths {
    pub install_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: Option<PathBuf>,
    pub service_config: Option<PathBuf>,
    pub backup_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupRecord {
    pub created_at: u64,
    pub version: String,
    pub install_backup: PathBuf,
    pub state_backup: Option<PathBuf>,
    pub config_backup: Option<PathBuf>,
    pub service_config_backup: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("requested version is not newer than installed version")]
    NotNewer,
    #[error("no compatible release artifact for target {0}")]
    UnsupportedTarget(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("command failed: {0}")]
    Command(String),
    #[error("update failed and rollback was attempted: {0}")]
    RolledBack(String),
}

impl ReleaseManifest {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.name != "GitRun" || !is_version(&self.version) || self.git_commit.is_empty() || self.artifacts.is_empty() {
            return Err(UpdateError::InvalidManifest("name/version/git_commit/artifacts are required".into()));
        }
        for artifact in &self.artifacts {
            if artifact.target.is_empty()
                || artifact.file.contains('/') || artifact.file.contains('\\')
                || !is_sha256(&artifact.sha256)
            {
                return Err(UpdateError::InvalidManifest(format!("invalid artifact {}", artifact.file)));
            }
        }
        for dependency in &self.dependencies {
            if dependency.name.is_empty() || dependency.minimum_version.is_empty() {
                return Err(UpdateError::InvalidManifest("dependency name/minimum_version are required".into()));
            }
        }
        if let Some(image) = &self.runner_image {
            if image.reference.is_empty() || !is_sha256_digest(&image.digest) || image.minimum_version.is_empty() {
                return Err(UpdateError::InvalidManifest("invalid runner image metadata".into()));
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, target: &str) -> Result<&ReleaseArtifact, UpdateError> {
        self.artifacts.iter().find(|a| a.target == target).ok_or_else(|| UpdateError::UnsupportedTarget(target.into()))
    }
}

pub fn load_manifest(path: impl AsRef<Path>) -> Result<ReleaseManifest, UpdateError> {
    let manifest: ReleaseManifest = serde_json::from_slice(&fs::read(path)?)?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn fetch_manifest(url: &str) -> Result<ReleaseManifest, UpdateError> {
    let client = Client::builder().user_agent("GitRun-Updater/0.3").build()?;
    let manifest = client.get(url).send()?.error_for_status()?.json()?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn latest_manifest(repository: &str) -> Result<ReleaseManifest, UpdateError> {
    let repo = repository.trim_end_matches('/');
    let api = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = Client::builder().user_agent("GitRun-Updater/0.3").build()?;
    match client.get(&api).send().and_then(|r| r.error_for_status()) {
        Ok(response) => {
            let release: serde_json::Value = response.json()?;
            let tag = release.get("tag_name").and_then(|v| v.as_str())
                .ok_or_else(|| UpdateError::InvalidManifest("GitHub release has no tag_name".into()))?;
            fetch_manifest(&format!("https://github.com/{repo}/releases/download/{tag}/release-manifest.json"))
        }
        Err(_) => {
            let version = client.get(format!("https://raw.githubusercontent.com/{repo}/main/version.txt"))
                .send()?.error_for_status()?.text()?.trim().to_owned();
            fetch_manifest(&format!("https://github.com/{repo}/releases/download/{version}/release-manifest.json"))
        }
    }
}

pub fn stage_update(manifest: &ReleaseManifest, current_version: &str, staging_root: impl AsRef<Path>) -> Result<PathBuf, UpdateError> {
    manifest.validate()?;
    if compare_versions(&manifest.version, current_version)? != std::cmp::Ordering::Greater {
        return Err(UpdateError::NotNewer);
    }
    let root = staging_root.as_ref();
    fs::create_dir_all(root)?;
    let marker = root.join("pending-update.json");
    let temp = root.join("pending-update.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(temp, &marker)?;
    Ok(marker)
}

pub fn build_plan(manifest: &ReleaseManifest, current_version: &str, target: &str, installed_dependencies: &[(String, Option<String>)]) -> Result<UpdatePlan, UpdateError> {
    manifest.validate()?;
    if compare_versions(&manifest.version, current_version)? != std::cmp::Ordering::Greater {
        return Err(UpdateError::NotNewer);
    }
    let artifact = manifest.artifact_for(target)?;
    let artifact_url = artifact.download_url.clone().or_else(|| {
        manifest.repository.as_ref().map(|repo| format!(
            "https://github.com/{repo}/releases/download/{}/{}",
            manifest.version, artifact.file
        ))
    }).ok_or_else(|| UpdateError::InvalidManifest("artifact download_url or repository is required".into()))?;

    let dependencies = manifest.dependencies.iter().map(|req| {
        let installed = installed_dependencies.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&req.name))
            .and_then(|(_, version)| version.clone());
        let compatible = installed.as_deref()
            .map(|v| compare_versions(v, &req.minimum_version).map(|o| o != std::cmp::Ordering::Less).unwrap_or(false))
            .unwrap_or(false);
        DependencyStatus {
            name: req.name.clone(),
            installed_version: installed,
            minimum_version: req.minimum_version.clone(),
            compatible,
            action: if compatible { "skip".into() } else { "update".into() },
        }
    }).collect();

    Ok(UpdatePlan {
        current_version: current_version.into(),
        target_version: manifest.version.clone(),
        target: target.into(),
        artifact: artifact.file.clone(),
        artifact_url,
        dependencies,
        runner_image: manifest.runner_image.clone(),
    })
}

pub fn download_and_verify(url: &str, expected_sha256: &str, destination: impl AsRef<Path>) -> Result<(), UpdateError> {
    let client = Client::builder().user_agent("GitRun-Updater/0.3").build()?;
    let mut response = client.get(url).send()?.error_for_status()?;
    let mut file = fs::File::create(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response.read(&mut buffer)?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256.to_ascii_lowercase() {
        return Err(UpdateError::ChecksumMismatch { expected: expected_sha256.into(), actual });
    }
    Ok(())
}

pub fn apply_update(paths: &UpdatePaths, archive: impl AsRef<Path>, target: &str, version: &str, health_check: bool) -> Result<BackupRecord, UpdateError> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let backup_dir = paths.backup_root.join(format!("{version}-{timestamp}"));
    fs::create_dir_all(&backup_dir)?;
    let install_backup = backup_dir.join("install");
    let state_backup = backup_dir.join("state");
    let config_backup = paths.config_dir.as_ref().map(|_| backup_dir.join("config"));
    let service_config_backup = paths.service_config.as_ref().map(|_| backup_dir.join("service-config"));

    if paths.install_dir.exists() {
        fs::rename(&paths.install_dir, &install_backup)?;
    }
    if paths.state_dir.exists() {
        copy_dir(&paths.state_dir, &state_backup)?;
    }
    if let Some(config_dir) = &paths.config_dir {
        if config_dir.exists() {
            copy_dir(config_dir, config_backup.as_ref().expect("config backup path"))?;
        }
    }
    if let Some(service_config) = &paths.service_config {
        if service_config.is_file() {
            if let Some(parent) = service_config_backup.as_ref().and_then(Path::parent) { fs::create_dir_all(parent)?; }
            fs::copy(service_config, service_config_backup.as_ref().expect("service config backup path"))?;
        } else if service_config.is_dir() {
            copy_dir(service_config, service_config_backup.as_ref().expect("service config backup path"))?;
        }
    }

    let staging = backup_dir.join("staging");
    if let Err(error) = extract_archive(archive.as_ref(), &staging)
        .and_then(|_| atomic_install(&staging, &paths.install_dir))
        .and_then(|_| if health_check { health_check_binary(&paths.install_dir, paths.config_dir.as_deref()) } else { Ok(()) })
    {
        rollback_install(paths, &install_backup, &state_backup, config_backup.as_deref(), paths.service_config.as_deref(), service_config_backup.as_deref())?;
        return Err(UpdateError::RolledBack(error.to_string()));
    }

    let record = BackupRecord {
        created_at: timestamp,
        version: version.into(),
        install_backup,
        state_backup: if state_backup.exists() { Some(state_backup) } else { None },
        config_backup,
        service_config_backup,
    };
    fs::write(backup_dir.join("backup.json"), serde_json::to_vec_pretty(&record)?)?;
    Ok(record)
}

pub fn rollback(paths: &UpdatePaths, backup: &BackupRecord) -> Result<(), UpdateError> {
    rollback_install(
        paths,
        &backup.install_backup,
        backup.state_backup.as_deref().unwrap_or_else(|| Path::new("")),
        backup.config_backup.as_deref(),
        paths.service_config.as_deref(),
        backup.service_config_backup.as_deref(),
    )
}

pub fn update_runner_image(image: &RunnerImage) -> Result<(), UpdateError> {
    let inspect = Command::new("docker").args(["image", "inspect", "--format", "{{json .RepoDigests}}", &image.reference]).output();
    let local_digest = inspect.ok().and_then(|output| if output.status.success() {
        String::from_utf8_lossy(&output.stdout).split('"').find(|v| v.starts_with("sha256:")).map(str::to_owned)
    } else { None });
    if local_digest.as_deref() == Some(image.digest.as_str()) {
        return Ok(());
    }
    run_command(Command::new("docker").args(["pull", &image.reference]))?;
    let output = Command::new("docker").args(["image", "inspect", "--format", "{{json .RepoDigests}}", &image.reference]).output()?;
    if !output.status.success() {
        return Err(UpdateError::Command("docker image inspect failed after pull".into()));
    }
    if !String::from_utf8_lossy(&output.stdout).contains(&image.digest) {
        return Err(UpdateError::Command(format!("runner image digest mismatch for {}", image.reference)));
    }
    Ok(())
}

pub fn refresh_docker_stack(compose_file: impl AsRef<Path>) -> Result<(), UpdateError> {
    run_command(Command::new("docker").args(["compose", "-f", compose_file.as_ref().to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid compose path"))?, "up", "-d", "--no-build", "--remove-orphans"]))
}

pub fn health_check_binary(install_dir: &Path, config_dir: Option<&Path>) -> Result<(), UpdateError> {
    let binary = if cfg!(windows) { install_dir.join("gitrun-rs.exe") } else { install_dir.join("gitrun-rs") };
    if !binary.is_file() {
        return Err(UpdateError::Command(format!("updated binary not found: {}", binary.display())));
    }
    let mut command = Command::new(binary);
    command.arg("doctor");
    if let Some(config) = config_dir { command.env("GITRUN_CONFIG_DIR", config); }
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!("post-update health check failed: {}", String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(())
}

pub fn update_incompatible_dependencies(plan: &UpdatePlan) -> Result<Vec<String>, UpdateError> {
    let mut updated = Vec::new();
    for dependency in &plan.dependencies {
        if dependency.action != "update" {
            continue;
        }
        update_dependency(&dependency.name)?;
        updated.push(dependency.name.clone());
    }
    Ok(updated)
}

fn update_dependency(name: &str) -> Result<(), UpdateError> {
    let command = match (std::env::consts::OS, name) {
        ("linux", "Git") => ("apt-get", vec!["install", "-y", "git"]),
        ("linux", "Docker") => ("apt-get", vec!["install", "-y", "docker.io", "docker-compose-plugin"]),
        ("linux", "Node.js") => ("apt-get", vec!["install", "-y", "nodejs"]),
        ("linux", "Python") => ("apt-get", vec!["install", "-y", "python3"]),
        ("macos", "Git") => ("brew", vec!["upgrade", "git"]),
        ("macos", "Docker") => ("brew", vec!["upgrade", "--cask", "docker"]),
        ("macos", "Node.js") => ("brew", vec!["upgrade", "node"]),
        ("macos", "Python") => ("brew", vec!["upgrade", "python"]),
        ("windows", "Git") => ("winget", vec!["upgrade", "--id", "Git.Git", "--accept-source-agreements", "--accept-package-agreements"]),
        ("windows", "Docker") => ("winget", vec!["upgrade", "--id", "Docker.DockerDesktop", "--accept-source-agreements", "--accept-package-agreements"]),
        ("windows", "Node.js") => ("winget", vec!["upgrade", "--id", "OpenJS.NodeJS", "--accept-source-agreements", "--accept-package-agreements"]),
        ("windows", "Python") => ("winget", vec!["upgrade", "--id", "Python.Python.3.12", "--accept-source-agreements", "--accept-package-agreements"]),
        _ => return Err(UpdateError::Command(format!("no package-manager strategy for {name} on {}", std::env::consts::OS))),
    };
    if command.0 == "apt-get" {
        let mut apt = Command::new("apt-get");
        apt.args(["update"]);
        if !is_root() {
            apt = Command::new("sudo");
            apt.args(["apt-get", "update"]);
        }
        run_command(&mut apt)?;
    }
    let mut update = if command.0 == "apt-get" && !is_root() {
        let mut sudo = Command::new("sudo");
        sudo.arg("apt-get").args(&command.1);
        sudo
    } else {
        let mut cmd = Command::new(command.0);
        cmd.args(&command.1);
        cmd
    };
    run_command(&mut update)
}

pub fn dependency_status(name: &str, minimum_version: &str) -> DependencyStatus {
    let installed = command_version(name);
    let compatible = installed.as_deref()
        .map(|v| compare_versions(v, minimum_version).map(|o| o != std::cmp::Ordering::Less).unwrap_or(false))
        .unwrap_or(false);
    DependencyStatus {
        name: name.into(),
        installed_version: installed,
        minimum_version: minimum_version.into(),
        compatible,
        action: if compatible { "skip".into() } else { "update".into() },
    }
}

fn is_root() -> bool {
    if cfg!(windows) {
        return false;
    }
    Command::new("id").args(["-u"]).output()
        .map(|output| output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

fn command_version(name: &str) -> Option<String> {
    let output = Command::new(name).arg("--version").output().ok()?;
    if !output.status.success() { return None; }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    extract_version(if stdout.is_empty() { &stderr } else { &stdout })
}

fn extract_version(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find(|p| p.split('.').count() >= 2 && p.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_owned)
}

fn compare_versions(a: &str, b: &str) -> Result<std::cmp::Ordering, UpdateError> {
    let parse = |v: &str| v.trim_start_matches('v').split('.').take(3)
        .map(|part| part.parse::<u64>().map_err(|_| UpdateError::InvalidManifest(format!("invalid version: {v}"))))
        .collect::<Result<Vec<_>, _>>();
    let av = parse(a)?;
    let bv = parse(b)?;
    Ok(av.cmp(&bv))
}

fn is_version(value: &str) -> bool {
    let trimmed = value.trim_start_matches('v');
    let base = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    base.split('.').count() == 3 && base.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}
fn is_sha256(value: &str) -> bool { value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) }
fn is_sha256_digest(value: &str) -> bool { value.starts_with("sha256:") && value.len() == 71 && value[7..].chars().all(|c| c.is_ascii_hexdigit()) }

fn extract_archive(archive: &Path, _target: &str, destination: &Path) -> Result<(), UpdateError> {
    fs::create_dir_all(destination)?;
    let name = archive.file_name().and_then(|v| v.to_str()).unwrap_or_default();
    if name.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(fs::File::open(archive)?);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err(UpdateError::InvalidManifest("archive contains path traversal".into()));
            }
            entry.unpack(destination)?;
        }
    } else if name.ends_with(".zip") {
        let file = fs::File::open(archive)?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| UpdateError::InvalidManifest(e.to_string()))?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|e| UpdateError::InvalidManifest(e.to_string()))?;
            let Some(enclosed) = entry.enclosed_name() else { return Err(UpdateError::InvalidManifest("archive contains unsafe path".into())); };
            let out = destination.join(enclosed);
            if entry.is_dir() { fs::create_dir_all(&out)?; } else {
                if let Some(parent) = out.parent() { fs::create_dir_all(parent)?; }
                io::copy(&mut entry, &mut fs::File::create(&out)?)?;
            }
        }
    } else {
        return Err(UpdateError::InvalidManifest("unsupported archive type".into()));
    }
    Ok(())
}

fn atomic_install(staging: &Path, install_dir: &Path) -> Result<(), UpdateError> {
    if install_dir.exists() {
        return Err(UpdateError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "install directory still exists after backup")));
    }
    fs::rename(staging, install_dir)?;
    Ok(())
}

fn rollback_install(paths: &UpdatePaths, install_backup: &Path, state_backup: &Path, config_backup: Option<&Path>, service_config: Option<&Path>, service_config_backup: Option<&Path>) -> Result<(), UpdateError> {
    if paths.install_dir.exists() { fs::remove_dir_all(&paths.install_dir)?; }
    if install_backup.exists() { fs::rename(install_backup, &paths.install_dir)?; }
    if paths.state_dir.exists() { fs::remove_dir_all(&paths.state_dir)?; }
    if state_backup.exists() { copy_dir(state_backup, &paths.state_dir)?; }
    if let (Some(config), Some(backup)) = (&paths.config_dir, config_backup) {
        if config.exists() { fs::remove_dir_all(config)?; }
        if backup.exists() { copy_dir(backup, config)?; }
    }
    if let (Some(service), Some(backup)) = (service_config, service_config_backup) {
        if service.exists() {
            if service.is_dir() { fs::remove_dir_all(service)?; } else { fs::remove_file(service)?; }
        }
        if backup.is_file() {
            if let Some(parent) = service.parent() { fs::create_dir_all(parent)?; }
            fs::copy(backup, service)?;
        } else if backup.is_dir() {
            copy_dir(backup, service)?;
        }
    }
    Ok(())
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), io::Error> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() { copy_dir(&src, &dst)?; } else { fs::copy(src, dst)?; }
    }
    Ok(())
}

fn run_command(command: &mut Command) -> Result<(), UpdateError> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!("{}: {}", output.status, String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            name: "GitRun".into(), version: "0.3.0".into(), git_commit: "abcdef1".into(),
            artifacts: vec![ReleaseArtifact {
                target: "x86_64-unknown-linux-gnu".into(),
                file: "GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz".into(),
                sha256: "a".repeat(64),
                download_url: Some("https://example.invalid/a".into()),
            }],
            dependencies: vec![DependencyRequirement {
                name: "Git".into(), minimum_version: "2.40.0".into(),
                recommended_version: Some("2.46.0".into()), source: None,
                installation_method: Some("system-package-manager".into()),
            }],
            runner_image: None, repository: Some("Vider06/GitRun".into()),
        }
    }
    #[test]
    fn rejects_same_or_older_version() {
        assert!(matches!(build_plan(&manifest(), "0.3.0", "x86_64-unknown-linux-gnu", &[]), Err(UpdateError::NotNewer)));
        assert!(matches!(build_plan(&manifest(), "0.4.0", "x86_64-unknown-linux-gnu", &[]), Err(UpdateError::NotNewer)));
    }
    #[test]
    fn selects_artifact_and_skips_compatible_dependency() {
        let plan = build_plan(&manifest(), "0.2.0", "x86_64-unknown-linux-gnu", &[("Git".into(), Some("2.45.0".into()))]).unwrap();
        assert_eq!(plan.artifact, "GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(plan.dependencies[0].action, "skip");
    }
    #[test]
    fn marks_missing_dependency_for_update() {
        let plan = build_plan(&manifest(), "0.2.0", "x86_64-unknown-linux-gnu", &[]).unwrap();
        assert_eq!(plan.dependencies[0].action, "update");
    }
    #[test]
    fn rejects_bad_runner_digest() {
        let mut m = manifest();
        m.runner_image = Some(RunnerImage { reference: "ghcr.io/vider06/gitrun-runner:v0.3.0".into(), digest: "bad".into(), minimum_version: "0.3.0".into() });
        assert!(m.validate().is_err());
    }
}
