//! Persistent registry for GitDockRun job/container bindings.
//!
//! The registry is intentionally small and host-side. It records logical
//! workflow-job bindings, not Docker permissions: GSR remains the authority
//! for every API operation. Atomic replacement makes scheduler/API concurrent
//! readers safe without introducing another database dependency.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const REGISTRY_FILE: &str = "dock-registry.json";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum DockRegistryError {
    #[error("unable to read dock registry: {0}")]
    Io(#[from] std::io::Error),
    #[error("unable to decode dock registry: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid dock registry: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockBinding {
    pub repository: String,
    pub run_id: u64,
    pub job: String,
    pub container: String,
    /// Immutable Docker object ID captured at connect time. An ID mismatch
    /// invalidates the binding and prevents container-name reuse/TOCTOU attacks.
    #[serde(default)]
    pub container_id: String,
    pub dynamic: bool,
    #[serde(default)]
    pub dock_only: bool,
    pub requester_runner: String,
    pub connected_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockRegistry {
    pub bindings: Vec<DockBinding>,
}

impl DockRegistry {
    pub fn load(state_dir: impl AsRef<Path>) -> Result<Self, DockRegistryError> {
        let state_dir = state_dir.as_ref();
        validate_state_directory(state_dir)?;
        let path = path_for(state_dir);
        let Some(raw) = read_registry_file(&path)? else {
            return Ok(Self::default());
        };
        let registry: Self = serde_json::from_str(&raw)?;
        registry.validate_unique_bindings()?;
        Ok(registry)
    }

    pub fn save(&self, state_dir: impl AsRef<Path>) -> Result<(), DockRegistryError> {
        let state_dir = state_dir.as_ref();
        fs::create_dir_all(state_dir)?;
        validate_state_directory(state_dir)?;
        let path = path_for(state_dir);
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence));
        let raw = serde_json::to_vec_pretty(self)?;

        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&raw)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }

        if let Err(error) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }

        Ok(())
    }

    fn validate_unique_bindings(&self) -> Result<(), DockRegistryError> {
        let mut keys = std::collections::HashSet::with_capacity(self.bindings.len());
        let mut containers = std::collections::HashSet::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            if binding.repository.trim().is_empty()
                || binding.run_id == 0
                || binding.job.trim().is_empty()
                || binding.container.trim().is_empty()
                || binding.requester_runner.trim().is_empty()
            {
                return Err(DockRegistryError::Invalid(
                    "binding contains an empty identity field or zero run ID".into(),
                ));
            }
            if !keys.insert((&binding.repository, binding.run_id, &binding.job)) {
                return Err(DockRegistryError::Invalid(
                    "duplicate repository/run/job binding".into(),
                ));
            }
            if !containers.insert(binding.container.as_str()) {
                return Err(DockRegistryError::Invalid(
                    "a Docker container is bound to more than one workflow job".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn binding(&self, repository: &str, run_id: u64, job: &str) -> Option<&DockBinding> {
        self.bindings.iter().find(|binding| {
            binding.repository == repository && binding.run_id == run_id && binding.job == job
        })
    }

    pub fn binding_for_container(&self, container: &str) -> Option<&DockBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.container == container)
    }

    /// Returns a binding only when the container belongs to the exact
    /// workflow-run/job trust domain and was connected by this runner.
    /// Container names and IDs are not authorization credentials on their own.
    pub fn binding_for_authorized_container(
        &self,
        repository: &str,
        run_id: u64,
        job: &str,
        requester_runner: &str,
        container: &str,
    ) -> Option<&DockBinding> {
        self.bindings.iter().find(|binding| {
            binding.repository == repository
                && binding.run_id == run_id
                && binding.job == job
                && binding.requester_runner == requester_runner
                && binding.container == container
        })
    }

    pub fn upsert(&mut self, binding: DockBinding) {
        self.bindings.retain(|existing| {
            !(existing.repository == binding.repository
                && existing.run_id == binding.run_id
                && existing.job == binding.job)
                && existing.container != binding.container
        });
        self.bindings.push(binding);
        self.bindings.sort_by(|a, b| {
            (&a.repository, a.run_id, &a.job).cmp(&(&b.repository, b.run_id, &b.job))
        });
    }

    pub fn remove(&mut self, repository: &str, run_id: u64, job: &str) -> Option<DockBinding> {
        let index = self.bindings.iter().position(|binding| {
            binding.repository == repository && binding.run_id == run_id && binding.job == job
        })?;
        Some(self.bindings.remove(index))
    }

    pub fn remove_missing_containers(&mut self, existing: impl Fn(&str) -> bool) -> bool {
        let original = self.bindings.len();
        self.bindings.retain(|binding| existing(&binding.container));
        original != self.bindings.len()
    }
}

fn validate_state_directory(state_dir: &Path) -> Result<(), DockRegistryError> {
    match fs::symlink_metadata(state_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(DockRegistryError::Invalid(
                    "registry state path must be a real directory, not a symlink or other file".into(),
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o022 != 0 {
                    return Err(DockRegistryError::Invalid(
                        "registry state directory must not be group- or world-writable".into(),
                    ));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_registry_file(path: &Path) -> Result<Option<String>, DockRegistryError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(DockRegistryError::Invalid(
                "registry file must be a regular file without hard links".into(),
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(DockRegistryError::Invalid(
                "registry file must not be group- or world-writable".into(),
            ));
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        return Ok(Some(raw));
    }
    #[cfg(not(unix))]
    {
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(DockRegistryError::Invalid(
                "registry path must be a regular file".into(),
            ));
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        Ok(Some(raw))
    }
}

pub fn path_for(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join(REGISTRY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn load_rejects_symlinked_registry_file() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("gitrun-dock-registry-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.json");
        fs::write(&target, r#"{"bindings":[]}"#).unwrap();
        symlink(&target, path_for(&dir)).unwrap();
        assert!(DockRegistry::load(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_hard_linked_registry_file() {
        use std::os::unix::fs::hard_link;
        let dir = std::env::temp_dir().join(format!("gitrun-dock-registry-hardlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = path_for(&dir);
        fs::write(&path, r#"{"bindings":[]}"#).unwrap();
        hard_link(&path, dir.join("alias.json")).unwrap();
        assert!(DockRegistry::load(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-dock-registry-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut registry = DockRegistry::default();
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 42,
            job: "cache".into(),
            container: "runner-cache".into(),
            container_id: "sha256:runner-cache".into(),
            dynamic: true,
            dock_only: false,
            requester_runner: "runner-main".into(),
            connected_at: 100,
        });
        registry.save(&dir).unwrap();

        let loaded = DockRegistry::load(&dir).unwrap();
        assert_eq!(loaded, registry);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_duplicate_workflow_job_bindings() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-dock-registry-duplicate-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = path_for(&dir);
        let duplicate = DockRegistry {
            bindings: vec![
                DockBinding {
                    repository: "owner/repo".into(),
                    run_id: 7,
                    job: "build".into(),
                    container: "runner-a".into(),
                    container_id: "id-a".into(),
                    dynamic: true,
                    dock_only: false,
                    requester_runner: "runner-main".into(),
                    connected_at: 1,
                },
                DockBinding {
                    repository: "owner/repo".into(),
                    run_id: 7,
                    job: "build".into(),
                    container: "runner-b".into(),
                    container_id: "id-b".into(),
                    dynamic: true,
                    dock_only: false,
                    requester_runner: "runner-main".into(),
                    connected_at: 2,
                },
            ],
        };
        fs::write(&path, serde_json::to_vec(&duplicate).unwrap()).unwrap();

        assert!(matches!(
            DockRegistry::load(&dir),
            Err(DockRegistryError::Invalid(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_replaces_stale_binding_for_reused_container_name() {
        let mut registry = DockRegistry::default();
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 1,
            job: "old-job".into(),
            container: "runner-a".into(),
            container_id: "old-id".into(),
            dynamic: true,
            dock_only: false,
            requester_runner: "runner-main".into(),
            connected_at: 1,
        });
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 2,
            job: "new-job".into(),
            container: "runner-a".into(),
            container_id: "new-id".into(),
            dynamic: false,
            dock_only: false,
            requester_runner: "runner-main".into(),
            connected_at: 2,
        });
        assert_eq!(registry.bindings.len(), 1);
        assert_eq!(registry.bindings[0].run_id, 2);
        assert_eq!(registry.bindings[0].container_id, "new-id");
    }

    #[test]
    fn upsert_replaces_same_workflow_job() {
        let mut registry = DockRegistry::default();
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 1,
            job: "cache".into(),
            container: "a".into(),
            container_id: "sha256:a".into(),
            dynamic: true,
            dock_only: false,
            requester_runner: "r1".into(),
            connected_at: 1,
        });
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 1,
            job: "cache".into(),
            container: "b".into(),
            container_id: "sha256:b".into(),
            dynamic: false,
            dock_only: true,
            requester_runner: "r2".into(),
            connected_at: 2,
        });
        assert_eq!(registry.bindings.len(), 1);
        assert_eq!(registry.bindings[0].container, "b");
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    fn binding(run_id: u64, runner: &str) -> DockBinding {
        DockBinding {
            repository: "Vider06/GitRun".into(),
            run_id,
            job: "build".into(),
            container: "dock-a".into(),
            container_id: "sha256:dock-a".into(),
            dynamic: true,
            dock_only: true,
            requester_runner: runner.into(),
            connected_at: 1,
        }
    }

    #[test]
    fn binding_authorization_rejects_cross_run() {
        let mut registry = DockRegistry::default();
        registry.upsert(binding(10, "runner-a"));
        assert!(registry
            .binding_for_authorized_container("Vider06/GitRun", 10, "build", "runner-a", "dock-a")
            .is_some());
        assert!(registry
            .binding_for_authorized_container("Vider06/GitRun", 11, "build", "runner-a", "dock-a")
            .is_none());
    }

    #[test]
    fn binding_authorization_rejects_cross_job() {
        let mut registry = DockRegistry::default();
        registry.upsert(binding(10, "runner-a"));
        assert!(registry
            .binding_for_authorized_container("Vider06/GitRun", 10, "deploy", "runner-a", "dock-a",)
            .is_none());
    }

    #[test]
    fn binding_authorization_rejects_cross_runner_and_container() {
        let mut registry = DockRegistry::default();
        registry.upsert(binding(10, "runner-a"));
        assert!(registry
            .binding_for_authorized_container("Vider06/GitRun", 10, "build", "runner-b", "dock-a")
            .is_none());
        assert!(registry
            .binding_for_authorized_container("Vider06/GitRun", 10, "build", "runner-a", "dock-b")
            .is_none());
    }
}
