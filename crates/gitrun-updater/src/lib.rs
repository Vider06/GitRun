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

const MAX_RELEASE_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseArtifact {
    pub target: String,
    pub file: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyRequirement {
    pub name: String,
    pub minimum_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
            || self
                .reference
                .rsplit_once('@')
                .is_some_and(|(_, reference_digest)| reference_digest != self.digest)
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
pub struct PremadeDockerfile {
    pub repository: String,
    pub reference: String,
    pub path: String,
}

impl PremadeDockerfile {
    pub fn validate(&self) -> Result<(), UpdateError> {
        validate_repository(&self.repository)?;
        if !is_full_git_commit(&self.reference) {
            return Err(UpdateError::InvalidManifest(
                "Premade Dockerfile reference must be a full immutable Git commit SHA".into(),
            ));
        }
        if self.path.trim().is_empty()
            || self.path.starts_with('/')
            || self.path.contains(['\\', '\n', '\r'])
            || self
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(UpdateError::InvalidManifest(
                "invalid Premade Dockerfile path".into(),
            ));
        }
        Ok(())
    }
}

pub const CANONICAL_PREMADE_RUNNER_REPOSITORY: &str = "Vider06/GitRun";
pub const CANONICAL_PREMADE_RUNNER_REF: &str = "8c12be2732ce8769b535394fc896c76d99d28472"; // DevSkim: ignore DS173237 because this is a public immutable Git commit SHA, not a secret.
pub const CANONICAL_PREMADE_RUNNER_PATH: &str = "Core/Dockers/runners/linux-x86_64/Dockerfile";

pub fn canonical_premade_runner_dockerfile() -> PremadeDockerfile {
    PremadeDockerfile {
        repository: CANONICAL_PREMADE_RUNNER_REPOSITORY.into(),
        reference: CANONICAL_PREMADE_RUNNER_REF.into(),
        path: CANONICAL_PREMADE_RUNNER_PATH.into(),
    }
}

pub fn fetch_premade_runner_dockerfile(source: &PremadeDockerfile) -> Result<String, UpdateError> {
    source.validate()?;
    let commit = if is_full_git_commit(&source.reference) {
        source.reference.clone()
    } else {
        resolve_premade_branch_head(&source.repository, &source.reference)?
    };
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        source.repository, commit, source.path
    );
    validate_https_url(&url)?;
    let content = http_client()?.get(url).send()?.error_for_status()?.text()?;
    if content.trim().is_empty() {
        return Err(UpdateError::InvalidManifest(
            "Premade Dockerfile is empty".into(),
        ));
    }
    if !content.contains("FROM ") || !content.contains("ENTRYPOINT ") {
        return Err(UpdateError::InvalidManifest(
            "Premade Dockerfile does not look like a runner Dockerfile".into(),
        ));
    }
    Ok(content)
}

pub fn sync_premade_runner_dockerfile(
    source: &PremadeDockerfile,
    destination: impl AsRef<Path>,
) -> Result<(), UpdateError> {
    let content = fetch_premade_runner_dockerfile(source)?;
    atomic_write_installed_file(destination.as_ref(), content.as_bytes(), 0o644)
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<DependencyRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_image: Option<RunnerImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        let mut seen_targets = std::collections::HashSet::new();
        let mut seen_files = std::collections::HashSet::new();
        for artifact in &self.artifacts {
            if artifact.target.trim().is_empty()
                || artifact.target.chars().any(char::is_control)
                || !is_safe_filename(&artifact.file)
                || !is_sha256(&artifact.sha256)
                || !seen_targets.insert(artifact.target.as_str())
                || !seen_files.insert(artifact.file.as_str())
            {
                return Err(UpdateError::InvalidManifest(format!(
                    "invalid or duplicate artifact target/file: {} ({})",
                    artifact.target, artifact.file
                )));
            }
            if let Some(url) = artifact.download_url.as_deref() {
                validate_https_url(url)?;
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

fn signature_required_from_env(value: Option<&str>) -> Result<bool, UpdateError> {
    match value.map(str::trim) {
        None | Some("") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Some(_) => Err(UpdateError::InvalidManifest(
            "GITRUN_UPDATE_SIGNATURE_REQUIRED must be true/false or 1/0".into(),
        )),
    }
}

impl ReleaseManifest {
    /// Verifies an Ed25519 signature. Signed manifests are required by
    /// default; setting GITRUN_UPDATE_SIGNATURE_REQUIRED=false is the explicit
    /// compatibility/unsafe opt-out for custom unsigned manifests.
    pub fn verify_signature_from_env(&self) -> Result<(), UpdateError> {
        let required = signature_required_from_env(
            std::env::var("GITRUN_UPDATE_SIGNATURE_REQUIRED")
                .ok()
                .as_deref(),
        )?;
        let key = std::env::var("GITRUN_UPDATE_PUBLIC_KEY_HEX").ok();
        let expected_key_id = std::env::var("GITRUN_UPDATE_PUBLIC_KEY_ID").ok();
        self.verify_signature(required, key.as_deref(), expected_key_id.as_deref())
    }

    fn verify_signature(
        &self,
        required: bool,
        public_key: Option<&str>,
        expected_key_id: Option<&str>,
    ) -> Result<(), UpdateError> {
        match (self.signature.as_deref(), public_key) {
            (None, _) if required => Err(UpdateError::InvalidManifest(
                "update manifest signature is required by default; set GITRUN_UPDATE_SIGNATURE_REQUIRED=false only for an explicit unsigned-update opt-out".into(),
            )),
            (None, _) => Ok(()),
            (Some(_), None) => Err(UpdateError::InvalidManifest(
                "manifest is signed but no public key is configured".into(),
            )),
            (Some(signature), Some(public_key)) => {
                if let Some(expected_id) = expected_key_id {
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

/// Builds a plan from a manifest that has already passed
/// load_manifest/fetch_manifest verification. Signature enforcement stays
/// at the untrusted-input boundaries, so this helper only handles planning.
pub fn build_plan(
    manifest: &ReleaseManifest,
    current_version: &str,
    target: &str,
    installed_dependencies: &[(String, Option<String>)],
) -> Result<UpdatePlan, UpdateError> {
    manifest.validate()?;
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
    reject_symlink_components(destination)?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::InvalidManifest(format!(
                "download destination must be a regular file: {}",
                destination.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = temporary_sibling(destination, ".gitrun-download");

    let result = (|| -> Result<(), UpdateError> {
        let mut response = http_client()?.get(url).send()?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RELEASE_ARTIFACT_BYTES)
        {
            return Err(UpdateError::InvalidManifest(format!(
                "release artifact exceeds the {}-byte download limit",
                MAX_RELEASE_ARTIFACT_BYTES
            )));
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut hasher = Sha256::new();
        let mut total_bytes = 0u64;
        let mut buffer = [0u8; 64 * 1024];

        loop {
            let read = response.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total_bytes = total_bytes
                .checked_add(read as u64)
                .ok_or_else(|| UpdateError::InvalidManifest("artifact size overflow".into()))?;
            if total_bytes > MAX_RELEASE_ARTIFACT_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "release artifact exceeds the {}-byte download limit",
                    MAX_RELEASE_ARTIFACT_BYTES
                )));
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
        file.sync_all()?;
        drop(file);

        let actual = hex_encode(hasher.finalize().as_ref());
        if actual != expected_sha256.to_ascii_lowercase() {
            return Err(UpdateError::ChecksumMismatch {
                expected: expected_sha256.to_ascii_lowercase(),
                actual,
            });
        }
        replace_temp_file(&temp, destination)?;
        sync_parent_directory(destination)?;
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
    let record = BackupRecord {
        created_at: timestamp as u64,
        version: version.into(),
        install_backup: install_backup.clone(),
        state_backup: if paths.state_dir.is_dir() {
            Some(state_backup.clone())
        } else {
            None
        },
        config_backup: config_backup.clone(),
        service_config_backup: service_config_backup.clone(),
    };

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
                copy_file_durable(
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

    // All copies are complete and the live installation is still untouched.
    // Persist recovery metadata before moving the old installation out of place.
    if let Err(error) = write_json_durable(&backup_dir.join("backup.json"), &record) {
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

    // The journal is already on disk, before any live state was changed.
    // Keep it for both manual rollback and post-crash recovery.
    sync_parent_directory(&backup_dir.join("backup.json"))?;
    Ok(record)
}

fn require_backup_path(
    path: &Path,
    label: &str,
    required: bool,
    directory: Option<bool>,
) -> Result<(), UpdateError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(UpdateError::InvalidManifest(format!(
                "rollback {label} is missing: {}",
                path.display()
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidManifest(format!(
            "rollback {label} must not be a symlink: {}",
            path.display()
        )));
    }
    if let Some(expect_directory) = directory {
        let correct_type = if expect_directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        };
        if !correct_type {
            return Err(UpdateError::InvalidManifest(format!(
                "rollback {label} has an unexpected filesystem type: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_backup_record_paths(
    paths: &UpdatePaths,
    backup: &BackupRecord,
) -> Result<PathBuf, UpdateError> {
    if !is_version(&backup.version) {
        return Err(UpdateError::InvalidManifest(
            "rollback record has an invalid version".into(),
        ));
    }
    let transaction_dir = paths
        .backup_root
        .join(format!("{}-{}", backup.version, backup.created_at));
    if backup.install_backup != transaction_dir.join("install") {
        return Err(UpdateError::InvalidManifest(
            "rollback install backup is outside its trusted transaction directory".into(),
        ));
    }
    if let Some(state) = backup.state_backup.as_deref() {
        if state != transaction_dir.join("state") {
            return Err(UpdateError::InvalidManifest(
                "rollback state backup is outside its trusted transaction directory".into(),
            ));
        }
    }
    let expected_config = paths
        .config_dir
        .as_ref()
        .map(|_| transaction_dir.join("config"));
    if backup.config_backup != expected_config {
        return Err(UpdateError::InvalidManifest(
            "rollback config backup does not match the configured transaction path".into(),
        ));
    }
    let expected_service = paths
        .service_config
        .as_ref()
        .map(|_| transaction_dir.join("service-config"));
    if backup.service_config_backup != expected_service {
        return Err(UpdateError::InvalidManifest(
            "rollback service backup does not match the configured transaction path".into(),
        ));
    }

    require_backup_path(&paths.backup_root, "backup root", true, Some(true))?;
    require_backup_path(&transaction_dir, "transaction directory", true, Some(true))?;
    // The original installation may not have existed when the update ran.
    require_backup_path(&backup.install_backup, "install backup", false, Some(true))?;
    if let Some(state) = backup.state_backup.as_deref() {
        require_backup_path(state, "state backup", true, Some(true))?;
    }
    if let Some(config) = backup.config_backup.as_deref() {
        require_backup_path(config, "config backup", false, Some(true))?;
    }
    if let Some(service) = backup.service_config_backup.as_deref() {
        require_backup_path(service, "service backup", false, None)?;
    }
    Ok(transaction_dir)
}

pub fn rollback(paths: &UpdatePaths, backup: &BackupRecord) -> Result<(), UpdateError> {
    validate_update_paths(paths)?;
    validate_backup_record_paths(paths, backup)?;
    let _lock = UpdateLock::acquire(&paths.backup_root)?;
    // Revalidate after acquiring the transaction lock and before any destructive
    // operation; serialized record paths are never accepted as destinations.
    validate_backup_record_paths(paths, backup)?;
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
    /// Trusted optional config modified by this update transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<PathBuf>,
    /// Backup of the original config, including its mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file_backup: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
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
    reject_symlink_components(backup_root)?;
    if paths_overlap(backup_root, version_file)
        || artifacts.iter().any(|artifact| {
            paths_overlap(backup_root, &artifact.destination)
                || paths_overlap(version_file, &artifact.destination)
        })
        || config_file.is_some_and(|config| {
            paths_overlap(backup_root, config)
                || paths_overlap(version_file, config)
                || artifacts
                    .iter()
                    .any(|artifact| paths_overlap(&artifact.destination, config))
        })
    {
        return Err(UpdateError::InvalidManifest(
            "backup, version metadata, config and artifact paths must not overlap".into(),
        ));
    }
    if let Some(config) = config_file {
        reject_symlink_components(config)?;
        match fs::symlink_metadata(config) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::InvalidManifest(format!(
                    "update config must be a regular file: {}",
                    config.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let archive_meta = fs::symlink_metadata(archive)?;
    if archive_meta.file_type().is_symlink() || !archive_meta.is_file() {
        return Err(UpdateError::InvalidManifest(
            "release archive must be a regular non-symlink file".into(),
        ));
    }
    if archive_meta.len() > MAX_RELEASE_ARTIFACT_BYTES {
        return Err(UpdateError::InvalidManifest(
            "release archive exceeds the configured download size limit".into(),
        ));
    }

    let _lock = UpdateLock::acquire(backup_root)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_dir = backup_root.join(format!("system-{version}-{timestamp}"));
    fs::create_dir(&backup_dir)?;
    let staging = backup_dir.join("staging");
    let mut backups = Vec::with_capacity(artifacts.len());

    let backup_result = (|| -> Result<(), UpdateError> {
        for (index, artifact) in artifacts.iter().enumerate() {
            let backup = match fs::symlink_metadata(&artifact.destination) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    let name = artifact
                        .destination
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("artifact");
                    let path = backup_dir.join(format!("artifact-{index}-{name}"));
                    copy_file_durable(&artifact.destination, &path)?;
                    Some(path)
                }
                Ok(_) => {
                    return Err(UpdateError::Command(format!(
                        "installed artifact is not a regular file: {}",
                        artifact.destination.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            backups.push(InstalledArtifactBackup {
                archive_name: artifact.archive_name.clone(),
                destination: artifact.destination.clone(),
                backup,
            });
        }

        match fs::symlink_metadata(version_file) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                copy_file_durable(version_file, &backup_dir.join("version.txt"))?;
            }
            Ok(_) => {
                return Err(UpdateError::Command(format!(
                    "version metadata is not a regular file: {}",
                    version_file.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(config) = config_file {
            match fs::symlink_metadata(config) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    copy_file_durable(config, &backup_dir.join("config.env"))?;
                }
                Ok(_) => {
                    return Err(UpdateError::Command(format!(
                        "update config is not a regular file: {}",
                        config.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();

    if let Err(error) = backup_result {
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    let version_file_backup = if backup_dir.join("version.txt").is_file() {
        Some(backup_dir.join("version.txt"))
    } else {
        None
    };
    let config_file_backup = config_file
        .filter(|_| backup_dir.join("config.env").is_file())
        .map(|_| backup_dir.join("config.env"));
    let record = InstalledBackupRecord {
        created_at: timestamp as u64,
        version: version.to_owned(),
        artifacts: backups,
        version_file: version_file.to_path_buf(),
        version_file_backup,
        config_file: config_file.map(Path::to_path_buf),
        config_file_backup,
    };

    // The rollback journal must be durable before the first live-file replacement.
    if let Err(error) = write_json_durable(&backup_dir.join("backup.json"), &record) {
        let _ = fs::remove_dir_all(&backup_dir);
        return Err(error);
    }

    let result = (|| -> Result<(), UpdateError> {
        extract_archive(archive, target, &staging)?;
        for artifact in artifacts {
            let source = staging.join(&artifact.archive_name);
            match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    atomic_replace_installed_file(&source, &artifact.destination, 0o755)?;
                }
                Ok(_) => {
                    return Err(UpdateError::InvalidManifest(format!(
                        "release artifact is not a regular file: {}",
                        artifact.archive_name
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound && !artifact.required => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(UpdateError::InvalidManifest(format!(
                        "release archive is missing required artifact {}",
                        artifact.archive_name
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }
        atomic_write_installed_file(version_file, version.as_bytes(), 0o644)?;
        health_check_installed_executable_expected(health_binary, config_file, version)?;
        Ok(())
    })();

    if let Err(error) = result {
        let rollback_result = rollback_installed_update_unchecked(&record);
        let _ = fs::remove_dir_all(&staging);
        if let Err(rollback_error) = rollback_result {
            return Err(UpdateError::RolledBack(format!(
                "{error}; rollback also failed: {rollback_error}; recovery journal: {}",
                backup_dir.join("backup.json").display()
            )));
        }
        return Err(UpdateError::RolledBack(format!(
            "{error}; installed files were rolled back; recovery journal: {}",
            backup_dir.join("backup.json").display()
        )));
    }

    let _ = fs::remove_dir_all(&staging);
    sync_parent_directory(&backup_dir.join("backup.json"))?;
    Ok(record)
}

pub fn rollback_installed_update(
    record: &InstalledBackupRecord,
    expected_artifacts: &[InstalledArtifact],
    expected_version_file: &Path,
    backup_root: &Path,
) -> Result<(), UpdateError> {
    rollback_installed_update_with_config(
        record,
        expected_artifacts,
        expected_version_file,
        None,
        backup_root,
    )
}

pub fn rollback_installed_update_with_config(
    record: &InstalledBackupRecord,
    expected_artifacts: &[InstalledArtifact],
    expected_version_file: &Path,
    expected_config_file: Option<&Path>,
    backup_root: &Path,
) -> Result<(), UpdateError> {
    validate_installed_rollback_record(
        record,
        expected_artifacts,
        expected_version_file,
        expected_config_file,
        backup_root,
    )?;
    let _lock = UpdateLock::acquire(backup_root)?;
    // Revalidate after acquiring the lock, immediately before any destructive operation.
    validate_installed_rollback_record(
        record,
        expected_artifacts,
        expected_version_file,
        expected_config_file,
        backup_root,
    )?;
    rollback_installed_update_unchecked(record)
}

fn validate_installed_rollback_record(
    record: &InstalledBackupRecord,
    expected_artifacts: &[InstalledArtifact],
    expected_version_file: &Path,
    expected_config_file: Option<&Path>,
    backup_root: &Path,
) -> Result<PathBuf, UpdateError> {
    if !is_version(&record.version) {
        return Err(UpdateError::InvalidManifest(
            "installed rollback record has an invalid version".into(),
        ));
    }
    if expected_artifacts.is_empty() || record.artifacts.len() != expected_artifacts.len() {
        return Err(UpdateError::InvalidManifest(
            "installed rollback record does not contain the exact expected artifact set".into(),
        ));
    }
    if record.version_file != expected_version_file {
        return Err(UpdateError::InvalidManifest(
            "installed rollback version-file path does not match the trusted installation".into(),
        ));
    }
    if let Some(record_config) = record.config_file.as_deref() {
        if expected_config_file != Some(record_config) {
            return Err(UpdateError::InvalidManifest(
                "installed rollback config path does not match the trusted installation".into(),
            ));
        }
    } else if record.config_file_backup.is_some() {
        return Err(UpdateError::InvalidManifest(
            "installed rollback record has a config backup without a destination".into(),
        ));
    }

    let transaction_dir =
        backup_root.join(format!("system-{}-{}", record.version, record.created_at));
    require_backup_path(backup_root, "backup root", true, Some(true))?;
    require_backup_path(
        &transaction_dir,
        "installed transaction directory",
        true,
        Some(true),
    )?;

    for (index, expected) in expected_artifacts.iter().enumerate() {
        let matches: Vec<_> = record
            .artifacts
            .iter()
            .filter(|item| {
                item.archive_name == expected.archive_name
                    && item.destination == expected.destination
            })
            .collect();
        if matches.len() != 1 {
            return Err(UpdateError::InvalidManifest(format!(
                "installed rollback artifact {:?} does not match the trusted destination",
                expected.archive_name
            )));
        }
        let item = matches[0];
        let file_name = expected
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                UpdateError::InvalidManifest(
                    "trusted installed artifact has no safe file name".into(),
                )
            })?;
        let expected_backup = transaction_dir.join(format!("artifact-{index}-{file_name}"));
        if let Some(backup) = item.backup.as_deref() {
            if backup != expected_backup {
                return Err(UpdateError::InvalidManifest(
                    "installed artifact backup is outside its trusted transaction directory".into(),
                ));
            }
            require_backup_path(backup, "installed artifact backup", true, Some(false))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if fs::metadata(backup)?.nlink() != 1 {
                    return Err(UpdateError::InvalidManifest(format!(
                        "installed artifact backup must not be hard-linked: {}",
                        backup.display()
                    )));
                }
            }
        } else {
            require_backup_path(
                &expected_backup,
                "unexpected installed artifact backup",
                false,
                Some(false),
            )?;
        }
        require_backup_path(
            &expected.destination,
            "installed artifact destination",
            false,
            None,
        )?;
        match fs::symlink_metadata(&expected.destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::InvalidManifest(format!(
                    "installed artifact destination is not a regular file: {}",
                    expected.destination.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let expected_version_backup = transaction_dir.join("version.txt");
    if let Some(backup) = record.version_file_backup.as_deref() {
        if backup != expected_version_backup {
            return Err(UpdateError::InvalidManifest(
                "installed version backup is outside its trusted transaction directory".into(),
            ));
        }
        require_backup_path(backup, "installed version backup", true, Some(false))?;
    } else {
        require_backup_path(
            &expected_version_backup,
            "unexpected installed version backup",
            false,
            Some(false),
        )?;
    }
    require_backup_path(
        expected_version_file,
        "installed version-file destination",
        false,
        None,
    )?;
    match fs::symlink_metadata(expected_version_file) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::InvalidManifest(format!(
                "installed version-file destination is not a regular file: {}",
                expected_version_file.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let expected_config_backup = transaction_dir.join("config.env");
    if let Some(config) = record.config_file.as_deref() {
        if let Some(backup) = record.config_file_backup.as_deref() {
            if backup != expected_config_backup {
                return Err(UpdateError::InvalidManifest(
                    "installed config backup is outside its trusted transaction directory".into(),
                ));
            }
            require_backup_path(backup, "installed config backup", true, Some(false))?;
        } else {
            require_backup_path(
                &expected_config_backup,
                "unexpected installed config backup",
                false,
                Some(false),
            )?;
        }
        require_backup_path(config, "installed config destination", false, Some(false))?;
    } else if record.config_file_backup.is_some() {
        return Err(UpdateError::InvalidManifest(
            "installed config backup has no destination".into(),
        ));
    }
    if let Some(config) = expected_config_file {
        reject_symlink_components(config)?;
        match fs::symlink_metadata(config) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::InvalidManifest(format!(
                    "trusted config destination is not a regular file: {}",
                    config.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(transaction_dir)
}

fn rollback_installed_update_unchecked(record: &InstalledBackupRecord) -> Result<(), UpdateError> {
    for artifact in &record.artifacts {
        reject_symlink_components(&artifact.destination)?;
        match fs::symlink_metadata(&artifact.destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::Command(format!(
                    "installed artifact destination is not a regular file: {}",
                    artifact.destination.display()
                )));
            }
            Ok(_) => fs::remove_file(&artifact.destination)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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
    reject_symlink_components(&record.version_file)?;
    match fs::symlink_metadata(&record.version_file) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::Command(format!(
                "version file path is not a regular file: {}",
                record.version_file.display()
            )));
        }
        Ok(_) => fs::remove_file(&record.version_file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(backup) = &record.version_file_backup {
        atomic_write_installed_file(&record.version_file, &fs::read(backup)?, 0o644)?;
    }

    if let Some(config) = &record.config_file {
        reject_symlink_components(config)?;
        match fs::symlink_metadata(config) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::Command(format!(
                    "config restore destination is not a regular file: {}",
                    config.display()
                )));
            }
            Ok(_) => fs::remove_file(config)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(backup) = &record.config_file_backup {
            copy_file_durable(backup, config)?;
        }
    }
    Ok(())
}

pub fn health_check_installed_binary(
    binary: &Path,
    config_file: Option<&Path>,
) -> Result<(), UpdateError> {
    // Preserve the public helper's full-health semantics for callers that
    // expect a doctor check; the transactional installer uses the narrower
    // executable-only check until system services have been reconciled.
    check_installed_binary(binary, config_file, None)?;
    let mut command = Command::new(binary);
    command.arg("doctor").arg("--no-cat");
    if let Some(config) = config_file {
        command.env("GITRUN_CONFIG_FILE", config);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(UpdateError::Command(format!(
            "post-update doctor check failed (exit {}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            if output.stdout.is_empty() {
                String::new()
            } else {
                format!(
                    "; stdout: {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                )
            }
        )));
    }
    Ok(())
}

fn health_check_installed_executable_expected(
    binary: &Path,
    config_file: Option<&Path>,
    expected_version: &str,
) -> Result<(), UpdateError> {
    check_installed_binary(binary, config_file, Some(expected_version))
}

fn check_installed_binary(
    binary: &Path,
    config_file: Option<&Path>,
    expected_version: Option<&str>,
) -> Result<(), UpdateError> {
    reject_symlink_components(binary)?;
    let metadata = fs::symlink_metadata(binary)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::Command(format!(
            "installed GitRun binary is not a regular file: {}",
            binary.display()
        )));
    }

    // The executable check must not require healthy services: that would roll
    // back a new binary before it has the chance to repair missing service units.
    let mut command = Command::new(binary);
    command.arg("--version");
    if let Some(config) = config_file {
        command.env("GITRUN_CONFIG_FILE", config);
    }
    let output = command.output()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || combined.trim().is_empty() {
        return Err(UpdateError::Command(format!(
            "post-update executable check failed (exit {}): {}",
            output.status,
            combined.trim()
        )));
    }
    if let Some(expected_version) = expected_version {
        if !combined.contains(expected_version) {
            return Err(UpdateError::Command(format!(
                "post-update version check failed: expected {expected_version}, received {}",
                combined.trim()
            )));
        }
    }
    Ok(())
}

fn atomic_replace_installed_file(
    source: &Path,
    destination: &Path,
    mode: u32,
) -> Result<(), UpdateError> {
    let source_meta = fs::symlink_metadata(source)?;
    if source_meta.file_type().is_symlink() || !source_meta.is_file() {
        return Err(UpdateError::InvalidManifest(format!(
            "staged artifact is not a regular file: {}",
            source.display()
        )));
    }
    reject_symlink_components(destination)?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::Command(format!(
                "installed artifact destination is not a regular file: {}",
                destination.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
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
        sync_parent_directory(destination)?;
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
    reject_symlink_components(destination)?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::Command(format!(
                "atomic write destination is not a regular file: {}",
                destination.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination
        .parent()
        .ok_or_else(|| UpdateError::Command("installed file has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp = temporary_sibling(destination, ".gitrun-update");
    let result = (|| -> Result<(), UpdateError> {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        output.write_all(content)?;
        output.sync_all()?;
        set_file_mode(&temp, mode)?;
        drop(output);
        replace_temp_file(&temp, destination)?;
        sync_parent_directory(destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn reject_symlink_components(path: &Path) -> Result<(), UpdateError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(UpdateError::InvalidManifest(format!(
                    "update path contains a symlink component: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sync_parent_directory(path: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn copy_file_durable(source: &Path, destination: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::InvalidManifest(format!(
            "refusing to back up a non-regular file: {}",
            source.display()
        )));
    }
    let mut input = fs::File::open(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    copy_permissions(&metadata, destination)?;
    drop(output);
    sync_parent_directory(destination)?;
    Ok(())
}

fn write_json_durable(path: &Path, value: &impl Serialize) -> Result<(), UpdateError> {
    atomic_write_installed_file(path, &serde_json::to_vec_pretty(value)?, 0o600)
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
    // Keep the configured runner reference immutable. A mutable tag can point
    // to different content between verification and container reconciliation.
    let pinned_reference = if image.reference.rsplit_once('@').is_some() {
        image.reference.clone()
    } else {
        format!("{}@{}", image.reference, image.digest)
    };
    let mut found = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        if line.trim_start().starts_with("GITRUN_RUNNER_IMAGE=") {
            lines.push(format!("GITRUN_RUNNER_IMAGE={pinned_reference}"));
            found = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("GITRUN_RUNNER_IMAGE={pinned_reference}"));
    }
    let mut output = lines.join("\n");
    output.push('\n');

    let mode = existing_file_mode(path, 0o644);
    atomic_write_installed_file(path, output.as_bytes(), mode)?;
    Ok(())
}

fn docker_repo_digests_contain(output: &str, expected_digest: &str) -> bool {
    output.split('"').any(|candidate| {
        candidate
            .rsplit_once('@')
            .map(|(_, digest)| digest == expected_digest)
            .unwrap_or(candidate == expected_digest)
    })
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
    let local_digest_matches = inspect
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            docker_repo_digests_contain(&String::from_utf8_lossy(&output.stdout), &image.digest)
        });
    if local_digest_matches {
        return Ok(());
    }
    let _ = fetch_premade_runner_dockerfile(&canonical_premade_runner_dockerfile())?;
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
        let apt_get = linux_system_command("apt-get")
            .ok_or_else(|| UpdateError::Command("trusted /usr/bin/apt-get was not found".into()))?;
        let mut apt = Command::new(apt_get);
        apt.args(["update"]);
        if !is_root() {
            let sudo = linux_system_command("sudo").ok_or_else(|| {
                UpdateError::Command("trusted /usr/bin/sudo was not found".into())
            })?;
            apt = Command::new(sudo);
            apt.args([apt_get, "update"]);
        }
        run_command(&mut apt)?;
    }
    let mut update = if command.0 == "apt-get" {
        let apt_get = linux_system_command("apt-get")
            .ok_or_else(|| UpdateError::Command("trusted /usr/bin/apt-get was not found".into()))?;
        if is_root() {
            let mut apt = Command::new(apt_get);
            apt.args(&command.1);
            apt
        } else {
            let sudo = linux_system_command("sudo").ok_or_else(|| {
                UpdateError::Command("trusted /usr/bin/sudo was not found".into())
            })?;
            let mut apt = Command::new(sudo);
            apt.arg(apt_get).args(&command.1);
            apt
        }
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

fn linux_system_command(name: &str) -> Option<&'static str> {
    match name {
        "apt-get" => Some("/usr/bin/apt-get"),
        "sudo" => Some("/usr/bin/sudo"),
        "id" => Some("/usr/bin/id"),
        _ => None,
    }
}

fn is_root() -> bool {
    if cfg!(windows) {
        return false;
    }
    let Some(id) = linux_system_command("id") else {
        return false;
    };
    Command::new(id)
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
    if !value.len().is_multiple_of(2) || !value.chars().all(|c| c.is_ascii_hexdigit()) {
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

fn extract_archive(
    archive_path: &Path,
    _target: &str,
    destination: &Path,
) -> Result<(), UpdateError> {
    let archive_meta = fs::symlink_metadata(archive_path)?;
    if archive_meta.file_type().is_symlink() || !archive_meta.is_file() {
        return Err(UpdateError::InvalidManifest(
            "release archive must be a regular non-symlink file".into(),
        ));
    }
    if archive_meta.len() > MAX_RELEASE_ARTIFACT_BYTES {
        return Err(UpdateError::InvalidManifest(
            "release archive exceeds the configured download size limit".into(),
        ));
    }
    reject_symlink_components(destination)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(UpdateError::InvalidManifest(format!(
                "staging destination already exists: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir_all(destination)?;
    let name = archive_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut seen_paths = std::collections::HashSet::new();
    let mut expanded_bytes = 0u64;

    if name.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(fs::File::open(archive_path)?);
        let mut archive = tar::Archive::new(decoder);
        let mut entry_count = 0usize;
        for entry in archive.entries()? {
            let mut entry = entry?;
            entry_count = entry_count.checked_add(1).ok_or_else(|| {
                UpdateError::InvalidManifest("archive entry count overflow".into())
            })?;
            if entry_count > MAX_ARCHIVE_ENTRIES {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive contains more than {MAX_ARCHIVE_ENTRIES} entries"
                )));
            }
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
            let entry_path = entry.path()?.into_owned();
            if !seen_paths.insert(entry_path.clone()) {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive contains duplicate path: {}",
                    entry_path.display()
                )));
            }
            let size = entry.size();
            if size > MAX_ARCHIVE_ENTRY_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive entry exceeds size limit: {}",
                    entry_path.display()
                )));
            }
            expanded_bytes = expanded_bytes.checked_add(size).ok_or_else(|| {
                UpdateError::InvalidManifest("expanded archive size overflow".into())
            })?;
            if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive expands beyond the {}-byte limit",
                    MAX_ARCHIVE_EXPANDED_BYTES
                )));
            }
            let mode = entry.header().mode().unwrap_or(0o644);
            if !entry.unpack_in(destination)? {
                return Err(UpdateError::InvalidManifest(
                    "archive entry escapes the update staging directory".into(),
                ));
            }
            let out = destination.join(&entry_path);
            if let Ok(metadata) = fs::symlink_metadata(&out) {
                if metadata.is_file() {
                    let safe_mode = ((mode & 0o755) | 0o600) & !0o6000;
                    set_file_mode(&out, safe_mode)?;
                } else if metadata.is_dir() {
                    let safe_mode = ((mode & 0o755) | 0o700) & !0o6000;
                    set_file_mode(&out, safe_mode)?;
                }
            }
        }
    } else if name.ends_with(".zip") {
        let file = fs::File::open(archive_path)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        if archive.len() > MAX_ARCHIVE_ENTRIES {
            return Err(UpdateError::InvalidManifest(format!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} entries"
            )));
        }
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
            let Some(relative) = entry.enclosed_name() else {
                return Err(UpdateError::InvalidManifest(
                    "archive contains unsafe path".into(),
                ));
            };
            if !seen_paths.insert(relative.clone()) {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive contains duplicate path: {}",
                    relative.display()
                )));
            }
            let size = entry.size();
            if size > MAX_ARCHIVE_ENTRY_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive entry exceeds size limit: {}",
                    relative.display()
                )));
            }
            expanded_bytes = expanded_bytes.checked_add(size).ok_or_else(|| {
                UpdateError::InvalidManifest("expanded archive size overflow".into())
            })?;
            if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "archive expands beyond the {}-byte limit",
                    MAX_ARCHIVE_EXPANDED_BYTES
                )));
            }
            let original_mode =
                entry
                    .unix_mode()
                    .unwrap_or(if entry.is_dir() { 0o755 } else { 0o644 });
            let file_type = original_mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(UpdateError::InvalidManifest(
                    "archive contains a link or special filesystem entry".into(),
                ));
            }
            let out = destination.join(&relative);
            if entry.is_dir() {
                fs::create_dir_all(&out)?;
                set_file_mode(&out, 0o755)?;
            } else {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&out)?;
                let copied = io::copy(
                    &mut entry.by_ref().take(size.saturating_add(1)),
                    &mut output,
                )?;
                if copied != size {
                    return Err(UpdateError::InvalidManifest(format!(
                        "archive entry size mismatch for {}: expected {size}, got {copied}",
                        relative.display()
                    )));
                }
                output.sync_all()?;
                let safe_mode = ((original_mode & 0o755) | 0o600) & !0o6000;
                set_file_mode(&out, safe_mode)?;
            }
        }
    } else {
        return Err(UpdateError::InvalidManifest(
            "unsupported archive type".into(),
        ));
    }
    sync_parent_directory(destination)?;
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
                copy_file_durable(backup, service)?;
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
        // A valid HTTPS starting URL is not enough: an HTTP downgrade during
        // redirect would still send the updater's request over cleartext.
        // Keep every redirect on HTTPS, including release-asset redirects.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_https_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("update redirects must remain on HTTPS")
            }
        }))
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("GitRun-Updater/0.3")
        .build()?)
}

fn is_https_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
}

fn validate_https_url(value: &str) -> Result<(), UpdateError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| UpdateError::InvalidManifest(format!("invalid HTTPS URL: {value}")))?;
    if !is_https_url(&url) {
        return Err(UpdateError::InvalidManifest(format!(
            "update URL must use HTTPS: {value}"
        )));
    }
    Ok(())
}

fn is_full_git_commit(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_premade_branch_head(repository: &str, reference: &str) -> Result<String, UpdateError> {
    if reference.starts_with("refs/") || reference.starts_with('/') {
        return Err(UpdateError::InvalidManifest(
            "Premade reference must be a branch, tag, or full commit".into(),
        ));
    }
    validate_repository(repository)?;
    let encoded_reference = reference.replace('/', "%2F");
    let url = format!("https://api.github.com/repos/{repository}/commits/{encoded_reference}");
    let value: serde_json::Value = http_client()?.get(url).send()?.error_for_status()?.json()?;
    let sha = value
        .get("sha")
        .and_then(|value| value.as_str())
        .filter(|value| is_full_git_commit(value))
        .ok_or_else(|| {
            UpdateError::InvalidManifest(
                "GitHub Premade reference did not resolve to a full commit SHA".into(),
            )
        })?;
    Ok(sha.to_owned())
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
        reject_symlink_components(path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(UpdateError::InvalidManifest(format!(
                    "{name} update path must not be a symlink"
                )));
            }
            Ok(metadata) => {
                if matches!(name, "state" | "backup") && !metadata.is_dir() {
                    return Err(UpdateError::InvalidManifest(format!(
                        "{name} update path must be a directory: {}",
                        path.display()
                    )));
                }
                if name == "install" && !metadata.is_dir() {
                    return Err(UpdateError::InvalidManifest(format!(
                        "install update path must be a directory: {}",
                        path.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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
            reject_symlink_components(path)?;
            if paths_overlap(path, &paths.backup_root)
                || paths_overlap(path, &paths.install_dir)
                || paths_overlap(path, &paths.state_dir)
            {
                return Err(UpdateError::InvalidManifest(format!(
                    "{name} update path overlaps a managed update path"
                )));
            }
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(UpdateError::InvalidManifest(format!(
                        "{name} update path must not be a symlink"
                    )));
                }
                Ok(metadata) if name == "config" && !metadata.is_dir() => {
                    return Err(UpdateError::InvalidManifest(format!(
                        "config update path must be a directory: {}",
                        path.display()
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
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
    let mut archive_names = std::collections::HashSet::new();
    let mut destinations = std::collections::HashSet::new();
    for artifact in artifacts {
        if !is_safe_filename(&artifact.archive_name)
            || !archive_names.insert(artifact.archive_name.as_str())
            || !destinations.insert(&artifact.destination)
        {
            return Err(UpdateError::InvalidManifest(format!(
                "invalid or duplicate installed artifact mapping: {} -> {}",
                artifact.archive_name,
                artifact.destination.display()
            )));
        }
        reject_symlink_components(&artifact.destination)?;
        match fs::symlink_metadata(&artifact.destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(UpdateError::Command(format!(
                    "installed artifact destination is not a regular file: {}",
                    artifact.destination.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if paths_overlap(version_file, &artifact.destination) {
            return Err(UpdateError::InvalidManifest(
                "version metadata path must not overlap an installed artifact".into(),
            ));
        }
    }
    reject_symlink_components(version_file)?;
    match fs::symlink_metadata(version_file) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::Command(format!(
                "version metadata is not a regular file: {}",
                version_file.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
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
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(root: &Path) -> Result<Self, UpdateError> {
        reject_symlink_components(root)?;
        fs::create_dir_all(root)?;
        set_file_mode(root, 0o700)?;
        let path = root.join(".gitrun-update.lock");
        reject_symlink_components(&path)?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            // A kernel lock is released on process exit or reboot. Keep the
            // lockfile inode instead of unlinking it, avoiding stale-lock
            // deadlocks and lock-inode races between competing update processes.
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .open(&path)?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(UpdateError::InvalidManifest(
                    "update lock path must be a regular non-symlink file".into(),
                ));
            }
            set_file_mode(&path, 0o600)?;
            // SAFETY: the descriptor belongs to the open regular file above
            // and remains owned by UpdateLock throughout the transaction.
            let lock_result = unsafe { native_flock(file.as_raw_fd(), 2 | 4) };
            if lock_result != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Err(UpdateError::Command(
                        "another GitRun update or rollback is already in progress".into(),
                    ));
                }
                return Err(error.into());
            }
            file.set_len(0)?;
            writeln!(file, "pid={}", std::process::id())?;
            file.sync_all()?;
            Ok(Self { _file: file })
        }

        #[cfg(not(unix))]
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            writeln!(file, "pid={}", std::process::id())?;
            file.sync_all()?;
            Ok(Self { path })
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "flock"]
    fn native_flock(fd: std::os::fd::RawFd, operation: i32) -> i32;
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
        // On Unix, dropping the open file releases flock in the kernel.
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
    fn privileged_linux_package_commands_use_fixed_system_paths() {
        assert_eq!(linux_system_command("apt-get"), Some("/usr/bin/apt-get"));
        assert_eq!(linux_system_command("sudo"), Some("/usr/bin/sudo"));
        assert_eq!(linux_system_command("id"), Some("/usr/bin/id"));
        assert_eq!(linux_system_command("custom"), None);
    }

    #[test]
    fn signature_requirement_defaults_to_required_and_accepts_explicit_values_only() {
        assert!(signature_required_from_env(None).unwrap());
        assert!(signature_required_from_env(Some("")).unwrap());
        assert!(signature_required_from_env(Some("true")).unwrap());
        assert!(signature_required_from_env(Some("TRUE")).unwrap());
        assert!(signature_required_from_env(Some("1")).unwrap());
        assert!(!signature_required_from_env(Some("false")).unwrap());
        assert!(!signature_required_from_env(Some("FALSE")).unwrap());
        assert!(!signature_required_from_env(Some("0")).unwrap());
        assert!(signature_required_from_env(Some("treu")).is_err());
        assert!(signature_required_from_env(Some("false ")).is_ok());
        assert!(signature_required_from_env(Some("yes")).is_err());
    }

    #[test]
    fn canonical_premade_runner_source_is_immutable() {
        let source = canonical_premade_runner_dockerfile();
        assert_eq!(source.repository, CANONICAL_PREMADE_RUNNER_REPOSITORY);
        assert_eq!(source.reference, CANONICAL_PREMADE_RUNNER_REF);
        assert_eq!(source.path, CANONICAL_PREMADE_RUNNER_PATH);
        assert!(source.validate().is_ok());
    }

    #[test]
    fn premade_runner_source_rejects_non_immutable_commit() {
        let mut source = canonical_premade_runner_dockerfile();
        source.reference = "refs/heads/main".into();
        assert!(source.validate().is_err());
    }

    #[test]
    fn signature_verification_accepts_valid_signature_and_rejects_tampering() {
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let seed = [7u8; 32];
        let key_pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let mut signed = manifest();
        signed.signature_key_id = Some("release-key-1".into());
        let payload = signed.signing_payload().unwrap();
        signed.signature = Some(
            key_pair
                .sign(&payload)
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        );
        let public_key: String = key_pair
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        assert!(signed
            .verify_signature(true, Some(&public_key), Some("release-key-1"))
            .is_ok());
        assert!(signed
            .verify_signature(true, Some(&public_key), Some("wrong-key"))
            .is_err());

        let mut tampered = signed.clone();
        tampered.version = "0.3.1".into();
        assert!(tampered
            .verify_signature(true, Some(&public_key), Some("release-key-1"))
            .is_err());
    }

    #[test]
    fn signature_verification_fails_closed_without_key_or_required_signature() {
        let unsigned = manifest();
        assert!(unsigned.verify_signature(true, None, None).is_err());
        assert!(unsigned.verify_signature(false, None, None).is_ok());

        let mut signed_without_key = unsigned;
        signed_without_key.signature = Some("a".repeat(128));
        assert!(signed_without_key
            .verify_signature(false, None, None)
            .is_err());
    }

    #[test]
    fn signing_payload_preserves_wire_shape_for_sparse_manifests() {
        let raw = r#"{
            "name": "GitRun",
            "version": "0.3.0",
            "git_commit": "abcdef1",
            "repository": "Vider06/GitRun",
            "artifacts": [
                {
                    "target": "x86_64-unknown-linux-gnu",
                    "file": "GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz",
                    "sha256": "fixture-sha256"
                }
            ],
            "dependencies": [
                {
                    "name": "Git",
                    "minimum_version": "2.40.0"
                }
            ]
        }"#;

        let manifest: ReleaseManifest = serde_json::from_str(raw).unwrap();
        let payload = String::from_utf8(manifest.signing_payload().unwrap()).unwrap();

        assert_eq!(
            payload,
            r#"{"artifacts":[{"file":"GitRun-v0.3.0-x86_64-unknown-linux-gnu.tar.gz","sha256":"fixture-sha256","target":"x86_64-unknown-linux-gnu"}],"dependencies":[{"minimum_version":"2.40.0","name":"Git"}],"git_commit":"abcdef1","name":"GitRun","repository":"Vider06/GitRun","version":"0.3.0"}"#
        );
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
    fn runner_image_digest_pins_are_enforced() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let image = RunnerImage {
            reference: "ghcr.io/vider06/gitrun-runner:stable".into(),
            digest: digest.clone(),
            minimum_version: "0.3.0".into(),
        };
        let root =
            std::env::temp_dir().join(format!("gitrun-updater-pin-image-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let config = root.join("gitrun.env");
        fs::write(&config, "GITRUN_RUNNER_IMAGE=old:tag\nOTHER_SETTING=1\n").unwrap();

        pin_runner_image(&config, &image).unwrap();
        let content = fs::read_to_string(&config).unwrap();
        assert!(content.contains(&format!(
            "GITRUN_RUNNER_IMAGE=ghcr.io/vider06/gitrun-runner:stable@{digest}"
        )));
        assert!(content.contains("OTHER_SETTING=1"));

        let mismatched = RunnerImage {
            reference: format!("ghcr.io/vider06/gitrun-runner@sha256:{}", "b".repeat(64)),
            digest,
            minimum_version: "0.3.0".into(),
        };
        assert!(mismatched.validate().is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn docker_repo_digest_matcher_reads_digest_after_repository_name() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let output = format!(r#"["ghcr.io/vider06/gitrun-runner:stable@{digest}"]"#);
        assert!(docker_repo_digests_contain(&output, &digest));
        assert!(!docker_repo_digests_contain(
            &output,
            &format!("sha256:{}", "b".repeat(64))
        ));
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
    fn update_redirect_targets_must_remain_https() {
        let secure = reqwest::Url::parse("https://github.com/releases/asset").unwrap();
        let mut downgrade = reqwest::Url::parse("https://example.invalid/asset").unwrap();
        downgrade.set_scheme("http").unwrap();
        assert!(is_https_url(&secure));
        assert!(!is_https_url(&downgrade));
    }

    #[test]
    fn rejects_http_artifact_url() {
        let mut m = manifest();
        let mut insecure = reqwest::Url::parse("https://example.invalid/a").unwrap();
        insecure.set_scheme("http").unwrap();
        m.artifacts[0].download_url = Some(insecure.to_string());
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
    fn rejects_tar_archive_entries_that_escape_staging_directory() {
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-tar-traversal-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let archive_path = root.join("malicious.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let payload = b"must not escape staging";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        // Write the hostile path directly into the tar header. The tar builder's
        // append_data validates paths and rejects traversal before the extractor
        // under test ever sees the archive.
        header.as_mut_bytes()[..14].copy_from_slice(b"../outside.txt");
        header.set_cksum();
        archive.append(&header, &payload[..]).unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let destination = root.join("destination");
        let result = extract_archive(&archive_path, "x86_64-unknown-linux-gnu", &destination);
        assert!(matches!(result, Err(UpdateError::InvalidManifest(message))
            if message.contains("escapes the update staging directory")));
        assert!(!root.join("outside.txt").exists());

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
    #[test]
    fn failed_health_check_rolls_back_a_partially_installed_update() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-apply-rollback-{}-{nonce}",
            std::process::id()
        ));
        let install = root.join("install");
        let state = root.join("state");
        let backup_root = root.join("backups");
        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(install.join("version.txt"), b"previous-version").unwrap();
        fs::write(install.join("keep-me"), b"previous-install").unwrap();
        fs::write(state.join("settings.json"), b"previous-state").unwrap();

        let archive_path = root.join("broken-update.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let payload = b"this archive intentionally omits the gitrun executable";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "unrelated.txt", &payload[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let paths = UpdatePaths {
            install_dir: install.clone(),
            state_dir: state.clone(),
            config_dir: None,
            service_config: None,
            backup_root: backup_root.clone(),
        };
        let result = apply_update(
            &paths,
            &archive_path,
            "x86_64-unknown-linux-gnu",
            "1.2.3",
            true,
        );
        assert!(
            matches!(result, Err(UpdateError::RolledBack(message))
                if message.contains("updated binary not found")),
            "missing updated executable should fail health-check and report rollback"
        );
        assert_eq!(
            fs::read(install.join("version.txt")).unwrap(),
            b"previous-version"
        );
        assert_eq!(
            fs::read(install.join("keep-me")).unwrap(),
            b"previous-install"
        );
        assert!(!install.join("unrelated.txt").exists());
        assert_eq!(
            fs::read(state.join("settings.json")).unwrap(),
            b"previous-state",
            "state must survive the failed transaction"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generic_rollback_restores_the_trusted_install_backup() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-rollback-restore-{}-{nonce}",
            std::process::id()
        ));
        let install = root.join("install");
        let state = root.join("state");
        let backup_root = root.join("backups");
        let transaction = backup_root.join("1.2.3-42");
        let install_backup = transaction.join("install");

        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&install_backup).unwrap();
        fs::write(install.join("version.txt"), b"new-version").unwrap();
        fs::write(install.join("new-only.txt"), b"new").unwrap();
        fs::write(install_backup.join("version.txt"), b"previous-version").unwrap();
        fs::write(state.join("preserve.txt"), b"state-is-not-being-restored").unwrap();

        let paths = UpdatePaths {
            install_dir: install.clone(),
            state_dir: state.clone(),
            config_dir: None,
            service_config: None,
            backup_root: backup_root.clone(),
        };
        let record = BackupRecord {
            created_at: 42,
            version: "1.2.3".into(),
            install_backup: install_backup.clone(),
            state_backup: None,
            config_backup: None,
            service_config_backup: None,
        };

        rollback(&paths, &record).expect("rollback should restore the trusted backup");

        assert_eq!(
            fs::read(install.join("version.txt")).unwrap(),
            b"previous-version"
        );
        assert!(!install.join("new-only.txt").exists());
        assert!(
            !install_backup.exists(),
            "trusted backup should be renamed into place"
        );
        assert_eq!(
            fs::read(state.join("preserve.txt")).unwrap(),
            b"state-is-not-being-restored"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generic_rollback_rejects_backup_paths_outside_trusted_transaction() {
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-rollback-paths-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let install = root.join("install");
        let state = root.join("state");
        let backup_root = root.join("backups");
        let transaction = backup_root.join("1.2.3-42");
        let attacker_install_backup = root.join("attacker-install");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("sentinel"), b"keep").unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&transaction).unwrap();
        fs::create_dir_all(&attacker_install_backup).unwrap();
        fs::write(attacker_install_backup.join("replacement"), b"untrusted").unwrap();

        let paths = UpdatePaths {
            install_dir: install.clone(),
            state_dir: state.clone(),
            config_dir: None,
            service_config: None,
            backup_root: backup_root.clone(),
        };
        let record = BackupRecord {
            created_at: 42,
            version: "1.2.3".into(),
            install_backup: attacker_install_backup,
            state_backup: None,
            config_backup: None,
            service_config_backup: None,
        };

        assert!(rollback(&paths, &record).is_err());
        assert_eq!(fs::read(install.join("sentinel")).unwrap(), b"keep");
        assert!(root.join("attacker-install/replacement").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn installed_rollback_rejects_untrusted_destination_before_side_effects() {
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-installed-rollback-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let backup_root = root.join("backups");
        let version = "1.2.3";
        let timestamp = 77;
        let transaction = backup_root.join(format!("system-{version}-{timestamp}"));
        let binary = root.join("trusted-install/gitrun");
        let version_file = root.join("trusted-share/version.txt");
        let attacker_target = root.join("attacker-target");
        fs::create_dir_all(&transaction).unwrap();
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::create_dir_all(version_file.parent().unwrap()).unwrap();
        fs::write(&binary, b"current binary").unwrap();
        fs::write(&version_file, b"1.2.3\n").unwrap();
        fs::write(&attacker_target, b"do not touch").unwrap();

        let expected = [InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: binary.clone(),
            required: true,
        }];
        let record = InstalledBackupRecord {
            created_at: timestamp,
            version: version.into(),
            artifacts: vec![InstalledArtifactBackup {
                archive_name: "gitrun".into(),
                destination: attacker_target.clone(),
                backup: None,
            }],
            version_file: version_file.clone(),
            version_file_backup: None,
            config_file: None,
            config_file_backup: None,
        };

        assert!(
            rollback_installed_update(&record, &expected, &version_file, &backup_root).is_err()
        );
        assert_eq!(fs::read(&binary).unwrap(), b"current binary");
        assert_eq!(fs::read(&attacker_target).unwrap(), b"do not touch");
        assert_eq!(fs::read(&version_file).unwrap(), b"1.2.3\n");
        let _ = fs::remove_dir_all(&root);
    }
    #[cfg(unix)]
    #[test]
    fn installed_update_persists_recovery_journal_and_can_restore_previous_artifacts() {
        use std::os::unix::fs::PermissionsExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-journal-{}-{nonce}",
            std::process::id()
        ));
        let binary = root.join("bin/gitrun");
        let version_file = root.join("share/version.txt");
        let backup_root = root.join("backups");
        let config_file = root.join("gitrun.env");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::create_dir_all(version_file.parent().unwrap()).unwrap();
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&version_file, b"0.9.0\n").unwrap();
        fs::write(&config_file, b"GITRUN_TEST=before\n").unwrap();
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o640)).unwrap();

        let archive_path = root.join("release.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let payload = b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"GitRun 1.2.3\"; exit 0; fi\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "gitrun", &payload[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let artifacts = [InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: binary.clone(),
            required: true,
        }];
        let record = apply_installed_update(
            &archive_path,
            "x86_64-unknown-linux-gnu",
            "1.2.3",
            &artifacts,
            &version_file,
            &backup_root,
            &binary,
            Some(&config_file),
        )
        .unwrap();

        let transaction =
            backup_root.join(format!("system-{}-{}", record.version, record.created_at));
        assert!(
            transaction.join("backup.json").is_file(),
            "journal must survive a completed install"
        );
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "1.2.3");
        assert_eq!(
            fs::read(record.artifacts[0].backup.as_ref().unwrap()).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );

        fs::write(&config_file, b"GITRUN_TEST=after\n").unwrap();
        rollback_installed_update_with_config(
            &record,
            &artifacts,
            &version_file,
            Some(&config_file),
            &backup_root,
        )
        .expect("the recovery journal must restore the binary, version and config");
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "0.9.0\n");
        assert_eq!(fs::read(&binary).unwrap(), b"#!/bin/sh\nexit 0\n");
        assert_eq!(fs::read(&config_file).unwrap(), b"GITRUN_TEST=before\n");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&config_file).unwrap().mode() & 0o777, 0o640);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn installed_update_rejects_symlink_destination_before_mutation() {
        use std::os::unix::fs::symlink;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitrun-updater-symlink-dest-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let outside = root.join("outside");
        let destination = root.join("gitrun");
        let version_file = root.join("version.txt");
        fs::write(&outside, b"must remain untouched").unwrap();
        symlink(&outside, &destination).unwrap();
        fs::write(&version_file, b"0.9.0\n").unwrap();

        let archive_path = root.join("release.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let payload = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "gitrun", &payload[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let artifacts = [InstalledArtifact {
            archive_name: "gitrun".into(),
            destination: destination.clone(),
            required: true,
        }];
        assert!(apply_installed_update(
            &archive_path,
            "x86_64-unknown-linux-gnu",
            "1.2.3",
            &artifacts,
            &version_file,
            &root.join("backups"),
            &destination,
            None,
        )
        .is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"must remain untouched");
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "0.9.0\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn release_manifest_rejects_duplicate_artifact_targets_and_files() {
        let mut duplicate_target = manifest();
        duplicate_target
            .artifacts
            .push(duplicate_target.artifacts[0].clone());
        assert!(duplicate_target.validate().is_err());

        let mut duplicate_file = manifest();
        let mut second = duplicate_file.artifacts[0].clone();
        second.target = "aarch64-unknown-linux-gnu".into();
        duplicate_file.artifacts.push(second);
        assert!(duplicate_file.validate().is_err());
    }
}
