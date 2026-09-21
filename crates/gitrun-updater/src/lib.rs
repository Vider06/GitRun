use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest { pub version: String, pub channel: String, pub platform: String, pub archive: String, pub sha256: String }

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("manifest I/O error: {0}")] Io(#[from] std::io::Error),
    #[error("manifest JSON error: {0}")] Json(#[from] serde_json::Error),
    #[error("refusing same-version update: {0}")] NotNewer(String),
}

pub fn load_manifest(path: impl AsRef<Path>) -> Result<ReleaseManifest, UpdateError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub fn stage_update(manifest: &ReleaseManifest, current_version: &str, staging_root: impl Into<PathBuf>) -> Result<PathBuf, UpdateError> {
    if manifest.version == current_version { return Err(UpdateError::NotNewer(current_version.into())); }
    let root = staging_root.into();
    fs::create_dir_all(&root)?;
    let marker = root.join("pending-update.json");
    fs::write(&marker, serde_json::to_vec_pretty(manifest)?)?;
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stages_pending_update() {
        let root = std::env::temp_dir().join(format!("gitrun-update-{}", std::process::id()));
        let m = ReleaseManifest { version: "0.3.0".into(), channel: "stable".into(), platform: "linux-x64".into(), archive: "gitrun.tar.gz".into(), sha256: "abc".into() };
        assert!(stage_update(&m, "0.2.0", &root).unwrap().exists());
        let _ = fs::remove_dir_all(root);
    }
}
