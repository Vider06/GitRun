use reqwest::blocking::Client;
use ring::signature::{UnparsedPublicKey, ED25519};
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

impl RunnerImage {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.reference.trim().is_empty()
            || self.reference.contains(['\r', '\n'])
            || self.reference.chars().any(char::is_whitespace)
            || !is_sha256_digest(&self.digest)
            || !is_version(&self.minimum_version)
        {
            return Err(UpdateError::InvalidManifest(
                "invalid runner image metadata".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub name: String,
    pub version: String,
    pub git_commit: String,
    pub artifacts: Vec<ReleaseArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_key_id: Option<String>,
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
        if self.name != "GitRun"
            || !is_version(&self.version)
            || self.git_commit.is_empty()
            || self.artifacts.is_empty()
        {
            return Err(UpdateError::InvalidManifest(
                "name/version/git_commit/artifacts are required".into(),
            ));
        }
        for artifact in &self.artifacts {
            if artifact.target.is_empty()
                || !is_safe_filename(&artifact.file)
                || !is_sha256(&artifact.sha256)
            {
                return Err(UpdateError::InvalidManifest(format!(
                    "invalid artifact {}",
                    artifact.file
                )));
            }
        }
        for dependency in &self.dependencies {
            if dependency.name.is_empty() || dependency.minimum_version.is_empty() {
                return Err(UpdateError::InvalidManifest(
                    "dependency name/minimum_version are required".into(),
                ));
            }
        }
        if let Some(image) = &self.runner_image {
            image.validate()?;
        }
        if let Some(repository) = &self.repository {
            validate_repository(repository)?;
        }
        match (&self.signature, &self.signature_key_id) {
            (Some(signature), key_id) => {
                if signature.len() != 128 || !signature.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(UpdateError::InvalidManifest(
                        "manifest signature must be 64-byte hex".into(),
                    ));
                }
                if key_id.as_ref().is_some_and(|id| id.trim().is_empty()) {
                    return Err(UpdateError::InvalidManifest(
                        "manifest signature key id cannot be empty".into(),
                    ));
                }
            }
            (None, Some(_)) => {
                return Err(UpdateError::InvalidManifest(
                    "signature key id requires a signature".into(),
                ))
            }
            (None, None) => {}
        }
        Ok(())
    }

    pub fn artifact_for(&self, target: &str) -> Result<&ReleaseArtifact, UpdateError> {
        self.artifacts
            .iter()
            .find(|a| a.target == target)
            .ok_or_else(|| UpdateError::UnsupportedTarget(target.into()))
    }
}

impl ReleaseManifest {
    /// Verifies an Ed25519 signature. Signed manifests are required by
    /// default; setting GITRUN_UPDATE_SIGNATURE_REQUIRED=false is the explicit
    /// compatibility/unsafe opt-out for custom unsigned manifests.
    pub fn verify_signature_from_env(&self) -> Result<(), UpdateError> {
        let required = std::env::var("GITRUN_UPDATE_SIGNATURE_REQUIRED")
            .ok()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(true);
        let key = std::env::var("GITRUN_UPDATE_PUBLIC_KEY_HEX").ok();
        match (self.signature.as_deref(), key.as_deref()) {
            (None, _) if required => Err(UpdateError::InvalidManifest(
                "update manifest signature is required by default; set GITRUN_UPDATE_SIGNATURE_REQUIRED=false only for an explicit unsigned-update opt-out".into(),
            )),
            (None, _) => Ok(()),
            (Some(_), None) => Err(UpdateError::InvalidManifest(
                "manifest is signed but no public key is configured".into(),
            )),
            (Some(signature), Some(public_key)) => {
                if let Ok(expected_id) = std::env::var("GITRUN_UPDATE_PUBLIC_KEY_ID") {
                    if self.signature_key_id.as_deref() != Some(expected_id.trim()) {
                        return Err(UpdateError::InvalidManifest(
                            "update manifest signing key id does not match configured key".into(),
                        ));
                    }
                }
                let public = decode_hex(public_key).ok_or_else(|| {
                    UpdateError::InvalidManifest("update public key must be 32-byte hex".into())
                })?;
                let sig = decode_hex(signature).ok_or_else(|| {
                    UpdateError::InvalidManifest("update signature must be hex".into())
                })?;
                if public.len() != 32 || sig.len() != 64 {
                    return Err(UpdateError::InvalidManifest(
                        "invalid Ed25519 key or signature length".into(),
                    ));
                }
                let payload = self.signing_payload()?;
                UnparsedPublicKey::new(&ED25519, &public)
                    .verify(&payload, &sig)
                    .map_err(|_| {
                        UpdateError::InvalidManifest(
                            "update manifest signature verification failed".into(),
                        )
                    })
            }
        }
    }

    fn signing_payload(&self) -> Result<Vec<u8>, UpdateError> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        unsigned.signature_key_id = None;
        let value = serde_json::to_value(&unsigned)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        let canonical = canonicalize_json(value);
        serde_json::to_vec(&canonical)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))
    }
}

pub fn load_manifest(path: impl AsRef<Path>) -> Result<ReleaseManifest, UpdateError> {
    let manifest: ReleaseManifest = serde_json::from_slice(&fs::read(path)?)?;
    manifest.validate()?;
    manifest.verify_signature_from_env()?;
    Ok(manifest)
}

pub fn fetch_manifest(url: &str) -> Result<ReleaseManifest, UpdateError> {
    validate_https_url(url)?;
    let client = http_client()?;
    let manifest: ReleaseManifest = client.get(url).send()?.error_for_status()?.json()?;
    manifest.validate()?;
    manifest.verify_signature_from_env()?;
    Ok(manifest)
}

pub fn latest_manifest(repository: &str) -> Result<ReleaseManifest, UpdateError> {
    validate_repository(repository)?;
    let repo = repository.trim_end_matches('/');
    let api = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = http_client()?;
    let response = client.get(&api).send()?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        let version = client
            .get(format!(
                "https://raw.githubusercontent.com/{repo}/main/version.txt"
            ))
            .send()?
            .error_for_status()?
            .text()?
            .trim()
            .to_owned();
        if !is_version(&version) {
            return Err(UpdateError::InvalidManifest(
                "fallback version.txt contains an invalid version".into(),
            ));
        }
        return fetch_manifest(&format!(
            "https://github.com/{repo}/releases/download/{version}/release-manifest.json"
        ));
    }

    let response = response.error_for_status()?;
    let release: serde_json::Value = response.json()?;
    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .filter(|value| is_version(value))
        .ok_or_else(|| {
            UpdateError::InvalidManifest("GitHub release has no valid tag_name".into())
        })?;

    fetch_manifest(&format!(
        "https://github.com/{repo}/releases/download/{tag}/release-manifest.json"
    ))
}

pub fn stage_update(
    manifest: &ReleaseManifest,
    current_version: &str,
    staging_root: impl AsRef<Path>,
) -> Result<PathBuf, UpdateError> {
    manifest.validate()?;
    manifest.verify_signature_from_env()?;
    if compare_versions(&manifest.version, current_version)? != std::cmp::Ordering::Greater {
        return Err(UpdateError::NotNewer);
    }
    let root = staging_root.as_ref();
    fs::create_dir_all(root)?;
    let _lock = UpdateLock::acquire(root)?;
    let marker = root.join("pending-update.json");
    let temp = temporary_sibling(&marker, ".gitrun-stage");
    let result = (|| -> Result<(), UpdateError> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.sync_all()?;
        drop(file);
        replace_temp_file(&temp, &marker)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(marker)
}

pub fn build_plan(
    manifest: &ReleaseManifest,
    current_version: &str,
    target: &str,
    installed_dependencies: &[(String, Option<String>)],
) -> Result<UpdatePlan, UpdateError> {
    manifest.validate()?;
    manifest.verify_signature_from_env()?;
    if compare_versions(&manifest.version, current_version)? != std::cmp::Ordering::Greater {
        return Err(UpdateError::NotNewer);
    }
    let artifact = manifest.artifact_for(target)?;
    let artifact_url = artifact
        .download_url
        .clone()
        .or_else(|| {
            manifest.repository.as_ref().map(|repo| {
                format!(
                    "https://github.com/{repo}/releases/download/{}/{}",
                    manifest.version, artifact.file
                )
            })
        })
        .ok_or_else(|| {
            UpdateError::InvalidManifest("artifact download_url or repository is required".into())
        })?;
    validate_https_url(&artifact_url)?;

    let dependencies = manifest
        .dependencies
        .iter()
        .map(|req| {
            let installed = installed_dependencies
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&req.name))
                .and_then(|(_, version)| version.clone());
            let compatible = installed
                .as_deref()
                .map(|v| {
                    compare_versions(v, &req.minimum_version)
                        .map(|o| o != std::cmp::Ordering::Less)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            DependencyStatus {
                name: req.name.clone(),
                installed_version: installed,
                minimum_version: req.minimum_version.clone(),
                compatible,
                action: if compatible {
                    "skip".into()
                } else {
                    "update".into()
                },
            }
        })
        .collect();

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

pub fn download_and_verify(
    url: &str,
    expected_sha256: &str,
    destination: impl AsRef<Path>,
) -> Result<(), UpdateError> {
    validate_https_url(url)?;
    if !is_sha256(expected_sha256) {
        return Err(UpdateError::InvalidManifest(
            "expected SHA-256 must be 64 hexadecimal characters".into(),
        ));
    }

    let destination = destination.as_ref();
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = temporary_sibling(destination, ".gitrun-download");

    let result = (|| -> Result<(), UpdateError> {
        let mut response = http_client()?.get(url).send()?.error_for_status()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];

        loop {
            let read = response.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
        file.sync_all()?;
        drop(file);

        let digest = hasher.finalize();
        let actual = hex_encode(digest.as_ref());
        if actual != expected_sha256.to_ascii_lowercase() {
            return Err(UpdateError::ChecksumMismatch {
                expected: expected_sha256.to_ascii_lowercase(),
                actual,
            });
        }

        replace_temp_file(&temp, destination)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn apply_update(
    paths: &UpdatePaths,
    archive: impl AsRef<Path>,
    target: &str,
    version: &str,
    health_check: bool,
) -> Result<BackupRecord, UpdateError> {
    validate_update_paths(paths)?;
    if !is_version(version) {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid update version: {version}"
        )));
    }

    let _lock = UpdateLock::acquire(&paths.backup_root)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_dir = paths.backup_root.join(format!("{version}-{timestamp}"));
    fs::create_dir_all(&backup_dir)?;

    let install_backup = backup_dir.join("install");
    let state_backup = backup_dir.join("state");
    let config_backup = paths.config_dir.as_ref().map(|_| backup_dir.join("config"));
    let service_config_backup = paths
        .service_config
        .as_ref()
        .map(|_| backup_dir.join("service-config"));

    let backup_result = (|| -> Result<(), UpdateError> {
        if paths.state_dir.exists() {
            copy_dir(&paths.state_dir, &state_backup)?;
        }
        if let Some(config_dir) = &paths.config_dir {
            if config_dir.exists() {
                copy_dir(
                    config_dir,
                    config_backup.as_ref().expect("config backup path"),
                )?;
            }
        }
        if let Some(service_config) = &paths.service_config {
            if service_config.is_file() {
                if let Some(parent) = service_config_backup
                    .as_ref()
                    .and_then(|path| path.parent())
                {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(
                    service_config,
                    service_config_backup
                        .as_ref()
                        .expect("service config backup path"),
                )?;
            } else if service_config.is_dir() {
                copy_dir(
                    service_config,
                    service_config_backup
                        .as_ref()
                        .expect("service config backup path"),
                )?;
            }
        }

        if paths.install_dir.exists() {
            fs::rename(&paths.install_dir, &install_backup)?;
        }
        Ok(())
    })();

    if let Err(error) = backup_result {
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    let staging = backup_dir.join("staging");
    let result = extract_archive(archive.as_ref(), target, &staging)
        .and_then(|_| atomic_install(&staging, &paths.install_dir))
        .and_then(|_| {
            if health_check {
                health_check_binary(&paths.install_dir, paths.config_dir.as_deref())
            } else {
                Ok(())
            }
        });

    if let Err(error) = result {
        if let Err(rollback_error) = rollback_install(
            paths,
            &install_backup,
            if state_backup.exists() {
                Some(state_backup.as_path())
            } else {
                None
            },
            config_backup.as_deref(),
            paths.service_config.as_deref(),
            service_config_backup.as_deref(),
            true,
        ) {
            return Err(UpdateError::RolledBack(format!(
                "{error}; rollback also failed: {rollback_error}"
            )));
        }
        return Err(UpdateError::RolledBack(error.to_string()));
    }

    let record = BackupRecord {
        created_at: timestamp as u64,
        version: version.into(),
        install_backup,
        state_backup: state_backup.exists().then_some(state_backup),
        config_backup,
        service_config_backup,
    };
    let backup_json = (|| -> Result<(), UpdateError> {
        let bytes = serde_json::to_vec_pretty(&record)?;
        fs::write(backup_dir.join("backup.json"), bytes)?;
        Ok(())
    })();

    if let Err(error) = backup_json {
        let rollback_result = rollback_install(
            paths,
            &record.install_backup,
            record.state_backup.as_deref(),
            record.config_backup.as_deref(),
            paths.service_config.as_deref(),
            record.service_config_backup.as_deref(),
            true,
        );
        if let Err(rollback_error) = rollback_result {
            return Err(UpdateError::RolledBack(format!(
                "backup record write failed: {error}; rollback also failed: {rollback_error}"
            )));
        }
        return Err(UpdateError::RolledBack(format!(
            "backup record write failed; GitRun was rolled back: {error}"
        )));
    }

    Ok(record)
}

pub fn rollback(paths: &UpdatePaths, backup: &BackupRecord) -> Result<(), UpdateError> {
    validate_update_paths(paths)?;
    let _lock = UpdateLock::acquire(&paths.backup_root)?;
    rollback_install(
        paths,
        &backup.install_backup,
        backup.state_backup.as_deref(),
        backup.config_backup.as_deref(),
        paths.service_config.as_deref(),
        backup.service_config_backup.as_deref(),
        false,
    )
}

#[derive(Debug, Clone)]
pub struct InstalledArtifact {
    pub archive_name: String,
    pub destination: PathBuf,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledArtifactBackup {
    pub archive_name: String,
    pub destination: PathBuf,
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledBackupRecord {
    pub created_at: u64,
    pub version: String,
    pub artifacts: Vec<InstalledArtifactBackup>,
    pub version_file: PathBuf,
    pub version_file_backup: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
pub fn apply_installed_update(
    archive: &Path,
    target: &str,
    version: &str,
    artifacts: &[InstalledArtifact],
    version_file: &Path,
    backup_root: &Path,
    health_binary: &Path,
    config_file: Option<&Path>,
) -> Result<InstalledBackupRecord, UpdateError> {
    if !is_version(version) {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid update version: {version}"
        )));
    }
    validate_installed_artifacts(artifacts, version_file)?;
    let _lock = UpdateLock::acquire(backup_root)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_dir = backup_root.join(format!("system-{version}-{timestamp}"));
    fs::create_dir_all(&backup_dir)?;
    let staging = backup_dir.join("staging");
    let mut backups = Vec::with_capacity(artifacts.len());

    let backup_result = (|| -> Result<(), UpdateError> {
        for artifact in artifacts {
            let backup = if artifact.destination.is_file() {
                let name = artifact
                    .destination
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("artifact");
                let path = backup_dir.join(format!("artifact-{name}"));
                fs::copy(&artifact.destination, &path)?;
                Some(path)
            } else {
                None
            };
            backups.push(InstalledArtifactBackup {
                archive_name: artifact.archive_name.clone(),
                destination: artifact.destination.clone(),
                backup,
            });
        }

        if let Ok(meta) = fs::symlink_metadata(version_file) {
            if meta.is_dir() {
                return Err(UpdateError::Command(format!(
                    "version file path is a directory: {}",
                    version_file.display()
                )));
            }
        }

        if version_file.is_file() {
            let path = backup_dir.join("version.txt");
            fs::copy(version_file, &path)?;
        }
        Ok(())
    })();

    if let Err(error) = backup_result {
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    let version_file_backup = if version_file.is_file() {
        Some(backup_dir.join("version.txt"))
    } else {
        None
    };

    let record = InstalledBackupRecord {
        created_at: timestamp as u64,
        version: version.to_owned(),
        artifacts: backups,
        version_file: version_file.to_path_buf(),
        version_file_backup,
    };

    let result = (|| -> Result<(), UpdateError> {
        extract_archive(archive, target, &staging)?;
        for artifact in artifacts {
            let source = staging.join(&artifact.archive_name);
            if !source.is_file() {
                if artifact.required {
                    return Err(UpdateError::InvalidManifest(format!(
                        "release archive is missing required artifact {}",
                        artifact.archive_name
                    )));
                }
                continue;
            }
            atomic_replace_installed_file(&source, &artifact.destination, 0o755)?;
        }
        atomic_write_installed_file(version_file, version.as_bytes(), 0o644)?;
        health_check_installed_binary(health_binary, config_file)?;
        Ok(())
    })();

    if let Err(error) = result {
        if let Err(rollback_error) = rollback_installed_update(&record) {
            return Err(UpdateError::RolledBack(format!(
                "{error}; rollback also failed: {rollback_error}"
            )));
        }
        return Err(UpdateError::RolledBack(error.to_string()));
    }

    let _ = fs::remove_dir_all(&staging);
    let backup_json = (|| -> Result<(), UpdateError> {
        let bytes = serde_json::to_vec_pretty(&record)?;
        fs::write(backup_dir.join("backup.json"), bytes)?;
        Ok(())
    })();

    if let Err(error) = backup_json {
        let rollback_result = rollback_installed_update(&record);
        if let Err(rollback_error) = rollback_result {
            return Err(UpdateError::RolledBack(format!(
                "backup record write failed: {error}; rollback also failed: {rollback_error}"
            )));
        }
        return Err(UpdateError::RolledBack(format!(
            "backup record write failed; installed update was rolled back: {error}"
        )));
    }

    Ok(record)
}

pub fn rollback_installed_update(record: &InstalledBackupRecord) -> Result<(), UpdateError> {
    for artifact in &record.artifacts {
        if let Ok(meta) = fs::symlink_metadata(&artifact.destination) {
            if meta.file_type().is_symlink() || meta.is_dir() {
                return Err(UpdateError::Command(format!(
                    "installed artifact destination is not a regular file: {}",
                    artifact.destination.display()
                )));
            }
        }
        if artifact.destination.exists() {
            fs::remove_file(&artifact.destination)?;
        }
        if let Some(backup) = &artifact.backup {
            let parent = artifact
                .destination
                .parent()
                .ok_or_else(|| UpdateError::Command("installed artifact has no parent".into()))?;
            fs::create_dir_all(parent)?;
            atomic_replace_installed_file(backup, &artifact.destination, 0o755)?;
        }
    }
    if let Ok(meta) = fs::symlink_metadata(&record.version_file) {
        if meta.file_type().is_symlink() || meta.is_dir() {
            return Err(UpdateError::Command(format!(
                "version file path is not a regular file: {}",
                record.version_file.display()
            )));
        }
        fs::remove_file(&record.version_file)?;
    }
    if let Some(backup) = &record.version_file_backup {
        if let Some(parent) = record.version_file.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_installed_file(&record.version_file, &fs::read(backup)?, 0o644)?;
    }
    Ok(())
}

pub fn health_check_installed_binary(
    binary: &Path,
    config_file: Option<&Path>,
) -> Result<(), UpdateError> {
    if !binary.is_file() {
        return Err(UpdateError::Command(format!(
            "installed GitRun binary not found: {}",
            binary.display()
        )));
    }
    let mut command = Command::new(binary);
    command.arg("doctor");
    if let Some(config) = config_file {
        command.env("GITRUN_CONFIG_FILE", config);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!(
            "post-update health check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn atomic_replace_installed_file(
    source: &Path,
    destination: &Path,
    mode: u32,
) -> Result<(), UpdateError> {
    if fs::symlink_metadata(source)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(UpdateError::InvalidManifest(format!(
            "staged artifact is a symlink: {}",
            source.display()
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| UpdateError::Command("installed artifact has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp = temporary_sibling(destination, ".gitrun-update");
    let result = (|| -> Result<(), UpdateError> {
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        set_file_mode(&temp, mode)?;
        drop(output);
        replace_temp_file(&temp, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn atomic_write_installed_file(
    destination: &Path,
    content: &[u8],
    mode: u32,
) -> Result<(), UpdateError> {
    let temp = temporary_sibling(destination, ".gitrun-update");
    let result = (|| -> Result<(), UpdateError> {
        let parent = destination
            .parent()
            .ok_or_else(|| UpdateError::Command("installed file has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        output.write_all(content)?;
        output.sync_all()?;
        set_file_mode(&temp, mode)?;
        drop(output);
        replace_temp_file(&temp, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn set_file_mode(path: &Path, mode: u32) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub fn pin_runner_image(
    config_file: impl AsRef<Path>,
    image: &RunnerImage,
) -> Result<(), UpdateError> {
    image.validate()?;
    let path = config_file.as_ref();
    let content = fs::read_to_string(path)?;
    let mut found = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        if line.trim_start().starts_with("GITRUN_RUNNER_IMAGE=") {
            lines.push(format!("GITRUN_RUNNER_IMAGE={}", image.reference));
            found = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("GITRUN_RUNNER_IMAGE={}", image.reference));
    }
    let mut output = lines.join("\n");
    output.push('\n');

    let mode = existing_file_mode(path, 0o644);
    atomic_write_installed_file(path, output.as_bytes(), mode)?;
    Ok(())
}

pub fn update_runner_image(image: &RunnerImage) -> Result<(), UpdateError> {
    image.validate()?;
    let inspect = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            &image.reference,
        ])
        .output();
    let local_digest = inspect.ok().and_then(|output| {
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout)
                .split('"')
                .find(|v| v.starts_with("sha256:"))
                .map(str::to_owned)
        } else {
            None
        }
    });
    if local_digest.as_deref() == Some(image.digest.as_str()) {
        return Ok(());
    }
    run_command(Command::new("docker").args(["pull", &image.reference]))?;
    let output = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            &image.reference,
        ])
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(
            "docker image inspect failed after pull".into(),
        ));
    }
    if !String::from_utf8_lossy(&output.stdout).contains(&image.digest) {
        return Err(UpdateError::Command(format!(
            "runner image digest mismatch for {}",
            image.reference
        )));
    }
    Ok(())
}

pub fn health_check_binary(
    install_dir: &Path,
    config_dir: Option<&Path>,
) -> Result<(), UpdateError> {
    let binary = if cfg!(windows) {
        [
            install_dir.join("gitrun.exe"),
            install_dir.join("gitrun-rs.exe"),
        ]
        .into_iter()
        .find(|path| path.is_file())
    } else {
        [install_dir.join("gitrun"), install_dir.join("gitrun-rs")]
            .into_iter()
            .find(|path| path.is_file())
    }
    .ok_or_else(|| {
        UpdateError::Command(format!(
            "updated binary not found in {}",
            install_dir.display()
        ))
    })?;
    let mut command = Command::new(binary);
    command.arg("doctor");
    if let Some(config) = config_dir {
        command.env("GITRUN_CONFIG_DIR", config);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!(
            "post-update health check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
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
        ("linux", "Docker") => (
            "apt-get",
            vec!["install", "-y", "docker.io", "docker-compose-plugin"],
        ),
        ("linux", "Node.js") => ("apt-get", vec!["install", "-y", "nodejs"]),
        ("linux", "Python") => ("apt-get", vec!["install", "-y", "python3"]),
        ("macos", "Git") => ("brew", vec!["upgrade", "git"]),
        ("macos", "Docker") => ("brew", vec!["upgrade", "--cask", "docker"]),
        ("macos", "Node.js") => ("brew", vec!["upgrade", "node"]),
        ("macos", "Python") => ("brew", vec!["upgrade", "python"]),
        ("windows", "Git") => (
            "winget",
            vec![
                "upgrade",
                "--id",
                "Git.Git",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        ),
        ("windows", "Docker") => (
            "winget",
            vec![
                "upgrade",
                "--id",
                "Docker.DockerDesktop",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        ),
        ("windows", "Node.js") => (
            "winget",
            vec![
                "upgrade",
                "--id",
                "OpenJS.NodeJS",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        ),
        ("windows", "Python") => (
            "winget",
            vec![
                "upgrade",
                "--id",
                "Python.Python.3.12",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        ),
        _ => {
            return Err(UpdateError::Command(format!(
                "no package-manager strategy for {name} on {}",
                std::env::consts::OS
            )))
        }
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
    let installed = dependency_command(name).and_then(command_version);
    let compatible = installed
        .as_deref()
        .map(|v| {
            compare_versions(v, minimum_version)
                .map(|o| o != std::cmp::Ordering::Less)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    DependencyStatus {
        name: name.into(),
        installed_version: installed,
        minimum_version: minimum_version.into(),
        compatible,
        action: if compatible {
            "skip".into()
        } else {
            "update".into()
        },
    }
}

fn dependency_command(name: &str) -> Option<&'static str> {
    let normalized = name.trim();
    match std::env::consts::OS {
        "linux" | "macos" => match normalized {
            "Git" | "git" => Some("git"),
            "Docker" | "docker" => Some("docker"),
            "Node.js" | "node" => Some("node"),
            "Python" | "python3" => Some("python3"),
            _ => None,
        },
        "windows" => match normalized {
            "Git" | "git" => Some("git"),
            "Docker" | "docker" => Some("docker"),
            "Node.js" | "node" => Some("node"),
            "Python" | "python3" | "python" => Some("python"),
            _ => None,
        },
        _ => None,
    }
}

fn is_root() -> bool {
    if cfg!(windows) {
        return false;
    }
    Command::new("id")
        .args(["-u"])
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0"
        })
        .unwrap_or(false)
}

fn command_version(name: &str) -> Option<String> {
    let output = Command::new(name).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    extract_version(if stdout.is_empty() { &stderr } else { &stdout })
}

fn extract_version(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find(|p| p.split('.').count() >= 2 && p.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionIdentifier {
    Numeric(u64),
    AlphaNumeric(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedVersion {
    core: [u64; 3],
    prerelease: Vec<VersionIdentifier>,
}

fn parse_version(value: &str) -> Result<ParsedVersion, UpdateError> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    if value.is_empty() {
        return Err(UpdateError::InvalidManifest(
            "invalid version: empty".into(),
        ));
    }

    let (without_build, _) = value
        .split_once('+')
        .map_or((value, None), |(base, build)| (base, Some(build)));

    if let Some((_, build)) = value.split_once('+') {
        if build.is_empty()
            || build.split('.').any(|part| {
                part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        {
            return Err(UpdateError::InvalidManifest(format!(
                "invalid version: {value}"
            )));
        }
    }

    let (core_text, prerelease_text) = match without_build.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (without_build, None),
    };

    let core_parts: Vec<&str> = core_text.split('.').collect();
    if core_parts.len() != 3 {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid version: {value}"
        )));
    }

    let mut core = [0u64; 3];
    for (index, part) in core_parts.into_iter().enumerate() {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(UpdateError::InvalidManifest(format!(
                "invalid version: {value}"
            )));
        }
        if part.len() > 1 && part.starts_with('0') {
            return Err(UpdateError::InvalidManifest(format!(
                "invalid version: {value}"
            )));
        }
        core[index] = part
            .parse::<u64>()
            .map_err(|_| UpdateError::InvalidManifest(format!("invalid version: {value}")))?;
    }

    let prerelease = prerelease_text
        .map(|text| {
            if text.is_empty() {
                return Err(UpdateError::InvalidManifest(format!(
                    "invalid version: {value}"
                )));
            }
            text.split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        return Err(UpdateError::InvalidManifest(format!(
                            "invalid version: {value}"
                        )));
                    }
                    if part.chars().all(|c| c.is_ascii_digit()) {
                        if part.len() > 1 && part.starts_with('0') {
                            return Err(UpdateError::InvalidManifest(format!(
                                "invalid version: {value}"
                            )));
                        }
                        Ok(VersionIdentifier::Numeric(part.parse::<u64>().map_err(
                            |_| UpdateError::InvalidManifest(format!("invalid version: {value}")),
                        )?))
                    } else {
                        Ok(VersionIdentifier::AlphaNumeric(part.to_owned()))
                    }
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(ParsedVersion { core, prerelease })
}

fn compare_versions(a: &str, b: &str) -> Result<std::cmp::Ordering, UpdateError> {
    let av = parse_version(a)?;
    let bv = parse_version(b)?;
    let core_cmp = av.core.cmp(&bv.core);
    if core_cmp != std::cmp::Ordering::Equal {
        return Ok(core_cmp);
    }

    match (av.prerelease.is_empty(), bv.prerelease.is_empty()) {
        (true, false) => return Ok(std::cmp::Ordering::Greater),
        (false, true) => return Ok(std::cmp::Ordering::Less),
        _ => {}
    }

    for (left, right) in av.prerelease.iter().zip(bv.prerelease.iter()) {
        let cmp = match (left, right) {
            (VersionIdentifier::Numeric(a), VersionIdentifier::Numeric(b)) => a.cmp(b),
            (VersionIdentifier::Numeric(_), VersionIdentifier::AlphaNumeric(_)) => {
                std::cmp::Ordering::Less
            }
            (VersionIdentifier::AlphaNumeric(_), VersionIdentifier::Numeric(_)) => {
                std::cmp::Ordering::Greater
            }
            (VersionIdentifier::AlphaNumeric(a), VersionIdentifier::AlphaNumeric(b)) => a.cmp(b),
        };
        if cmp != std::cmp::Ordering::Equal {
            return Ok(cmp);
        }
    }

    Ok(av.prerelease.len().cmp(&bv.prerelease.len()))
}

fn is_version(value: &str) -> bool {
    parse_version(value).is_ok()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(entries) => {
            let sorted: std::collections::BTreeMap<_, _> = entries
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect();
            serde_json::to_value(sorted).unwrap_or(serde_json::Value::Null)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for index in (0..bytes.len()).step_by(2) {
        let high = (bytes[index] as char).to_digit(16)? as u8;
        let low = (bytes[index + 1] as char).to_digit(16)? as u8;
        output.push((high << 4) | low);
    }
    Some(output)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn is_sha256_digest(value: &str) -> bool {
    value.starts_with("sha256:")
        && value.len() == 71
        && value[7..].chars().all(|c| c.is_ascii_hexdigit())
}

fn extract_archive(archive: &Path, _target: &str, destination: &Path) -> Result<(), UpdateError> {
    fs::create_dir_all(destination)?;
    let name = archive
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default();

    if name.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(fs::File::open(archive)?);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            if kind.is_symlink()
                || kind.is_hard_link()
                || kind.is_character_special()
                || kind.is_block_special()
                || kind.is_fifo()
            {
                return Err(UpdateError::InvalidManifest(
                    "archive contains links or special filesystem entries".into(),
                ));
            }
            entry.unpack_in(destination)?;
        }
    } else if name.ends_with(".zip") {
        let file = fs::File::open(archive)?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|e| UpdateError::InvalidManifest(e.to_string()))?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| UpdateError::InvalidManifest(e.to_string()))?;
            let Some(enclosed) = entry.enclosed_name() else {
                return Err(UpdateError::InvalidManifest(
                    "archive contains unsafe path".into(),
                ));
            };
            let out = destination.join(enclosed);
            if entry.is_dir() {
                fs::create_dir_all(&out)?;
            } else {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&out)?;
                io::copy(&mut entry, &mut output)?;
                output.sync_all()?;
            }
        }
    } else {
        return Err(UpdateError::InvalidManifest(
            "unsupported archive type".into(),
        ));
    }
    Ok(())
}

fn atomic_install(staging: &Path, install_dir: &Path) -> Result<(), UpdateError> {
    if install_dir.exists() {
        return Err(UpdateError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "install directory still exists after backup",
        )));
    }
    fs::rename(staging, install_dir)?;
    Ok(())
}

fn rollback_install(
    paths: &UpdatePaths,
    install_backup: &Path,
    state_backup: Option<&Path>,
    config_backup: Option<&Path>,
    service_config: Option<&Path>,
    service_config_backup: Option<&Path>,
    remove_current_without_backup: bool,
) -> Result<(), UpdateError> {
    if install_backup.exists() {
        remove_path(&paths.install_dir)?;
        fs::rename(install_backup, &paths.install_dir)?;
    } else if remove_current_without_backup && paths.install_dir.exists() {
        remove_path(&paths.install_dir)?;
    }

    match state_backup {
        Some(backup) if backup.exists() => {
            remove_path(&paths.state_dir)?;
            copy_dir(backup, &paths.state_dir)?;
        }
        Some(_) if remove_current_without_backup && paths.state_dir.exists() => {
            remove_path(&paths.state_dir)?;
        }
        _ => {}
    }

    if let (Some(config), Some(backup)) = (&paths.config_dir, config_backup) {
        if backup.exists() {
            remove_path(config)?;
            copy_dir(backup, config)?;
        } else if remove_current_without_backup && config.exists() {
            remove_path(config)?;
        }
    }

    if let (Some(service), Some(backup)) = (service_config, service_config_backup) {
        if backup.exists() {
            remove_path(service)?;
            if backup.is_file() {
                if let Some(parent) = service.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(backup, service)?;
            } else if backup.is_dir() {
                copy_dir(backup, service)?;
            }
        } else if remove_current_without_backup && service.exists() {
            remove_path(service)?;
        }
    }
    Ok(())
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to copy symlink: {}", source.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("backup source is not a directory: {}", source.display()),
        ));
    }

    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        let entry_meta = fs::symlink_metadata(&src)?;

        if entry_meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to copy symlink: {}", src.display()),
            ));
        }

        if entry_meta.is_dir() {
            copy_dir(&src, &dst)?;
        } else if entry_meta.is_file() {
            fs::copy(&src, &dst)?;
            copy_permissions(&entry_meta, &dst)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported filesystem entry in backup: {}", src.display()),
            ));
        }
    }
    copy_permissions(&metadata, destination)?;
    Ok(())
}

fn http_client() -> Result<Client, UpdateError> {
    Ok(Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("GitRun-Updater/0.3")
        .build()?)
}

fn validate_https_url(value: &str) -> Result<(), UpdateError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| UpdateError::InvalidManifest(format!("invalid HTTPS URL: {value}")))?;
    if url.scheme() != "https" {
        return Err(UpdateError::InvalidManifest(format!(
            "update URL must use HTTPS: {value}"
        )));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<(), UpdateError> {
    let trimmed = repository.trim_end_matches('/');
    let mut parts = trimmed.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty()
        || name.is_empty()
        || parts.next().is_some()
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(UpdateError::InvalidManifest(format!(
            "invalid GitHub repository: {repository}"
        )));
    }
    Ok(())
}

fn validate_update_paths(paths: &UpdatePaths) -> Result<(), UpdateError> {
    let managed = [
        ("install", paths.install_dir.as_path()),
        ("state", paths.state_dir.as_path()),
        ("backup", paths.backup_root.as_path()),
    ];
    for (name, path) in managed {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                return Err(UpdateError::InvalidManifest(format!(
                    "{name} update path must not be a symlink"
                )));
            }
        }
    }
    for i in 0..managed.len() {
        for j in (i + 1)..managed.len() {
            if paths_overlap(managed[i].1, managed[j].1) {
                return Err(UpdateError::InvalidManifest(format!(
                    "{} and {} update paths overlap",
                    managed[i].0, managed[j].0
                )));
            }
        }
    }
    let optional = [
        ("config", paths.config_dir.as_deref()),
        ("service-config", paths.service_config.as_deref()),
    ];
    for (name, path) in optional {
        if let Some(path) = path {
            if paths_overlap(path, &paths.backup_root)
                || paths_overlap(path, &paths.install_dir)
                || paths_overlap(path, &paths.state_dir)
            {
                return Err(UpdateError::InvalidManifest(format!(
                    "{name} update path overlaps a managed update path"
                )));
            }
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.file_type().is_symlink() {
                    return Err(UpdateError::InvalidManifest(format!(
                        "{name} update path must not be a symlink"
                    )));
                }
            }
        }
    }
    if let (Some(config), Some(service)) =
        (paths.config_dir.as_deref(), paths.service_config.as_deref())
    {
        if paths_overlap(config, service) {
            return Err(UpdateError::InvalidManifest(
                "config and service-config update paths overlap".into(),
            ));
        }
    }
    Ok(())
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn is_safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':'])
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == value)
}

fn validate_installed_artifacts(
    artifacts: &[InstalledArtifact],
    version_file: &Path,
) -> Result<(), UpdateError> {
    if artifacts.is_empty() {
        return Err(UpdateError::InvalidManifest(
            "installed update requires at least one artifact".into(),
        ));
    }
    for artifact in artifacts {
        if artifact.archive_name.is_empty()
            || artifact.archive_name.contains(['/', '\\'])
            || artifact.archive_name == "."
            || artifact.archive_name == ".."
        {
            return Err(UpdateError::InvalidManifest(format!(
                "invalid installed artifact archive name: {}",
                artifact.archive_name
            )));
        }
        if let Ok(meta) = fs::symlink_metadata(&artifact.destination) {
            if meta.file_type().is_symlink() {
                return Err(UpdateError::Command(format!(
                    "installed artifact destination is a symlink: {}",
                    artifact.destination.display()
                )));
            }
            if meta.is_dir() {
                return Err(UpdateError::Command(format!(
                    "installed artifact destination is a directory: {}",
                    artifact.destination.display()
                )));
            }
        }
    }
    if let Ok(meta) = fs::symlink_metadata(version_file) {
        if meta.file_type().is_symlink() {
            return Err(UpdateError::Command(format!(
                "version file is a symlink: {}",
                version_file.display()
            )));
        }
        if meta.is_dir() {
            return Err(UpdateError::Command(format!(
                "version file path is a directory: {}",
                version_file.display()
            )));
        }
    }
    Ok(())
}

fn temporary_sibling(path: &Path, prefix: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        "{prefix}-{name}-{}-{nonce}.tmp",
        std::process::id()
    ))
}

fn replace_temp_file(temp: &Path, destination: &Path) -> Result<(), UpdateError> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(temp, destination)?;
    sync_parent_directory(destination)?;
    Ok(())
}

fn sync_parent_directory(path: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn existing_file_mode(path: &Path, default_mode: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|meta| meta.permissions().mode())
            .unwrap_or(default_mode)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        default_mode
    }
}

fn remove_path(path: &Path) -> Result<(), io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_permissions(metadata: &fs::Metadata, destination: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.permissions().mode()),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, destination);
    }
    Ok(())
}

struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(root: &Path) -> Result<Self, UpdateError> {
        fs::create_dir_all(root)?;
        let path = root.join(".gitrun-update.lock");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        writeln!(file, "pid={}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn run_command(command: &mut Command) -> Result<(), UpdateError> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            name: "GitRun".into(),
            version: "0.3.0".into(),
            git_commit: "abcdef1".into(),
            artifacts: vec![ReleaseArtifact {
                target: "x86_64-unknown-linux-gnu".into(),
                file: "GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz".into(),
                sha256: "a".repeat(64),
                download_url: Some("https://example.invalid/a".into()),
            }],
            dependencies: vec![DependencyRequirement {
                name: "Git".into(),
                minimum_version: "2.40.0".into(),
                recommended_version: Some("2.46.0".into()),
                source: None,
                installation_method: Some("system-package-manager".into()),
            }],
            signature: None,
            signature_key_id: None,
            runner_image: None,
            repository: Some("Vider06/GitRun".into()),
        }
    }

    #[test]
    fn rejects_same_or_older_version() {
        assert!(matches!(
            build_plan(&manifest(), "0.3.0", "x86_64-unknown-linux-gnu", &[]),
            Err(UpdateError::NotNewer)
        ));
        assert!(matches!(
            build_plan(&manifest(), "0.4.0", "x86_64-unknown-linux-gnu", &[]),
            Err(UpdateError::NotNewer)
        ));
    }

    #[test]
    fn selects_artifact_and_skips_compatible_dependency() {
        let plan = build_plan(
            &manifest(),
            "0.2.0",
            "x86_64-unknown-linux-gnu",
            &[("Git".into(), Some("2.45.0".into()))],
        )
        .unwrap();
        assert_eq!(
            plan.artifact,
            "GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
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
        m.runner_image = Some(RunnerImage {
            reference: "ghcr.io/vider06/gitrun-runner:v0.3.0".into(),
            digest: "bad".into(),
            minimum_version: "0.3.0".into(),
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn version_comparison_supports_prerelease_and_rejects_truncation() {
        assert_eq!(
            compare_versions("1.2.3-alpha", "1.2.3").unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("1.2.3", "1.2.3-alpha").unwrap(),
            std::cmp::Ordering::Greater
        );
        assert!(compare_versions("1.2.3.4", "1.2.3").is_err());
    }

    #[test]
    fn rejects_invalid_runner_reference() {
        let mut m = manifest();
        m.runner_image = Some(RunnerImage {
            reference: "ghcr.io/vider06/image\nGITRUN_INJECTED=1".into(),
            digest: "sha256:".to_owned() + &"a".repeat(64),
            minimum_version: "0.3.0".into(),
        });
        assert!(m.validate().is_err());
    }

    #[test]
    fn rejects_http_artifact_url() {
        let mut m = manifest();
        m.artifacts[0].download_url = Some("http://example.invalid/a".into());
        assert!(build_plan(&m, "0.2.0", "x86_64-unknown-linux-gnu", &[]).is_err());
    }

    #[test]
    fn dependency_status_does_not_execute_arbitrary_commands() {
        let status = dependency_status("sh -c 'echo pwned'", "0.0.0");
        assert!(status.installed_version.is_none());
        assert_eq!(status.action, "update");
    }

    #[cfg(unix)]
    #[test]
    fn health_check_binary_accepts_unified_gitrun_binary() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("gitrun-updater-health-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let binary = root.join("gitrun");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

        health_check_binary(&root, None).unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extracts_zip_archive() {
        let root =
            std::env::temp_dir().join(format!("gitrun-updater-zip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let archive_path = root.join("update.zip");
        let destination = root.join("destination");

        let file = fs::File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("bin/gitrun", options).unwrap();
        writer.write_all(b"gitrun-test").unwrap();
        writer.finish().unwrap();

        extract_archive(&archive_path, "x86_64-unknown-linux-gnu", &destination).unwrap();

        assert_eq!(
            fs::read(destination.join("bin/gitrun")).unwrap(),
            b"gitrun-test"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_overlapping_update_paths() {
        let root = std::env::temp_dir().join(format!("gitrun-updater-test-{}", std::process::id()));
        let paths = UpdatePaths {
            install_dir: root.join("install"),
            state_dir: root.join("state"),
            config_dir: None,
            service_config: None,
            backup_root: root.join("install/backups"),
        };
        assert!(validate_update_paths(&paths).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-symlink-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("source")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        fs::write(root.join("outside/data"), b"x").unwrap();
        symlink(root.join("outside/data"), root.join("source/link")).unwrap();

        let result = copy_dir(&root.join("source"), &root.join("dest"));
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&root);
    }
}
