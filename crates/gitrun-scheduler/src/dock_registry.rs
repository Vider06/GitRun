//! Persistent registry for GitDockRun job/container bindings.
//!
//! The registry is intentionally small and host-side. It records logical
//! workflow-job bindings, not Docker permissions: GSR remains the authority
//! for every API operation. Atomic replacement makes scheduler/API concurrent
//! readers safe without introducing another database dependency.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

const REGISTRY_FILE: &str = "dock-registry.json";

#[derive(Debug, Error)]
pub enum DockRegistryError {
    #[error("unable to read dock registry: {0}")]
    Io(#[from] std::io::Error),
    #[error("unable to decode dock registry: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockBinding {
    pub repository: String,
    pub run_id: u64,
    pub job: String,
    pub container: String,
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
        let path = path_for(state_dir);
        match fs::read_to_string(path) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, state_dir: impl AsRef<Path>) -> Result<(), DockRegistryError> {
        let state_dir = state_dir.as_ref();
        fs::create_dir_all(state_dir)?;
        let path = path_for(state_dir);
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
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

pub fn path_for(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join(REGISTRY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn upsert_replaces_same_workflow_job() {
        let mut registry = DockRegistry::default();
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 1,
            job: "cache".into(),
            container: "a".into(),
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
