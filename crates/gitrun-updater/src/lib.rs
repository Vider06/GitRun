use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub version: String,
    pub channel: String,
    pub platform: String,
    pub archive: String,
    pub sha256: String,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("manifest I/O error: {0}")] Io(#[from] std::io::Error),
    #[error("manifest JSON error: {0}")] Json(#[from] serde_json::Error),
    #[error("invalid manifest: {0}")] InvalidManifest(String),
    #[error("refusing update: requested version is not newer")] NotNewer,
}

impl ReleaseManifest {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.version.is_empty() || self.channel.is_empty() || self.platform.is_empty() ||
           self.archive.is_empty() || self.sha256.len() != 64 ||
           !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UpdateError::InvalidManifest("version/channel/platform/archive/sha256 must be valid".into()));
        }
        Ok(())
    }
}

pub fn load_manifest(path: impl AsRef<Path>) -> Result<ReleaseManifest, UpdateError> {
    let manifest: ReleaseManifest = serde_json::from_slice(&fs::read(path)?)?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn stage_update(manifest: &ReleaseManifest, current_version: &str, staging_root: impl Into<PathBuf>) -> Result<PathBuf, UpdateError> {
    manifest.validate()?;
    if manifest.version <= current_version { return Err(UpdateError::NotNewer); }
    let root = staging_root.into();
    fs::create_dir_all(&root)?;
    let marker = root.join("pending-update.json");
    let temp = root.join("pending-update.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(temp, &marker)?;
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_checksum() {
        let m = ReleaseManifest { version:"0.3.0".into(), channel:"stable".into(), platform:"linux-x64".into(), archive:"gitrun.tar.gz".into(), sha256:"bad".into() };
        assert!(m.validate().is_err());
    }
}
