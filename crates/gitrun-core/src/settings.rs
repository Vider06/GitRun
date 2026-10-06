//! Persistent GitRun security/settings policy.
//!
//! Settings are deliberately separate from Config: Config contains
//! daemon/bootstrap configuration, while this file contains workflow
//! capability policy that may later be edited by the dashboard or CLI.
//!
//! The hierarchy is:
//!
//! `global policy` -> `repository restrictions` -> `effective policy`
//!
//! Repository settings can only narrow the global policy. They cannot
//! re-enable an API or operation that the global policy disabled.

use crate::api_policy::{ApiPolicy, GitRunApi, GitRunOperation, PolicyMatrix};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
const SETTINGS_FILE_NAME: &str = "gitrun-settings.json";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("unable to read settings: {0}")]
    Io(#[from] std::io::Error),
    #[error("unable to decode settings: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("unable to encode settings: {0}")]
    Encode(#[source] serde_json::Error),
}

/// Global policy plus per-repository restrictions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRunSettings {
    pub schema_version: u32,
    pub global: PolicyMatrix,
    pub repositories: BTreeMap<String, RepositorySettings>,
}

impl Default for GitRunSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            global: PolicyMatrix::secure_default(),
            repositories: BTreeMap::new(),
        }
    }
}

impl GitRunSettings {
    pub fn effective_for_repository(&self, repository: &str) -> EffectiveRepositorySettings {
        let repo = self
            .repositories
            .get(repository)
            .cloned()
            .unwrap_or_default();
        let api_policy = apply_repository_api_overrides(&self.global, &repo.api_overrides);
        EffectiveRepositorySettings {
            api_policy,
            docker: repo.docker,
            vault: repo.vault,
            storage: repo.storage,
            register: repo.register,
        }
    }

    pub fn path_for_state_dir(state_dir: impl AsRef<Path>) -> PathBuf {
        state_dir.as_ref().join(SETTINGS_FILE_NAME)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let raw = fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(SettingsError::Decode)
    }

    /// Load existing settings, or return a secure fail-closed policy when
    /// the settings file has not been created yet.
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        match Self::load(path) {
            Ok(settings) => Ok(settings),
            Err(SettingsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    /// Atomic replacement of the policy file. The file is not secret data,
    /// but policy tampering is security-sensitive, so Unix deployments keep
    /// it owner-readable/writable only.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SettingsError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let raw = serde_json::to_vec_pretty(self).map_err(SettingsError::Encode)?;
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));

        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&raw)?;
            file.sync_all()?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        }

        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temp);
                Err(error.into())
            }
        }
    }
}

fn apply_repository_api_overrides(
    global: &PolicyMatrix,
    overrides: &BTreeMap<GitRunApi, ApiPolicyOverride>,
) -> PolicyMatrix {
    let mut effective = global.clone();

    for api in GitRunApi::ALL {
        let Some(override_policy) = overrides.get(&api) else {
            continue;
        };

        let base = effective.get(api);
        let narrower = override_policy.to_policy(&base);
        effective.set(api, base.intersection(&narrower));
    }

    effective
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPolicyOverride {
    /// None means inherit global enablement; Some(false) can only disable.
    pub enabled: Option<bool>,
    /// None means inherit all globally-allowed operations; Some(...) can only
    /// remove operations through intersection.
    pub allowed_operations: Option<BTreeSet<GitRunOperation>>,
}

impl ApiPolicyOverride {
    fn to_policy(&self, base: &ApiPolicy) -> ApiPolicy {
        ApiPolicy {
            enabled: self.enabled.unwrap_or(base.enabled),
            allowed_operations: self
                .allowed_operations
                .clone()
                .unwrap_or_else(|| base.allowed_operations.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySettings {
    pub api_overrides: BTreeMap<GitRunApi, ApiPolicyOverride>,
    pub docker: DockerPolicy,
    pub vault: VaultPolicy,
    pub storage: SharedStoragePolicy,
    pub register: RegisterPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerPolicy {
    /// Direct Docker socket exposure to workflow containers. This is the
    /// compatibility opt-out and remains disabled unless explicitly enabled
    /// for this repository.
    pub direct_socket_enabled: bool,
    /// Logical job names that may be connected with GitDockRun --job.
    /// Empty means none; * is an explicit opt-in for all logical jobs.
    pub allowed_job_names: Vec<String>,
    /// Logical container names that may be targeted by --docked.
    /// Empty means none; * is an explicit opt-in for all logical containers.
    pub allowed_container_names: Vec<String>,
    /// Per logic-container restrictions. These are evaluated after the API
    /// policy, so a repository can expose GitDockRun while still blocking one
    /// particular container or one of its operations.
    pub logic_containers: BTreeMap<String, LogicContainerPolicy>,
    pub allowed_mounts: MountPolicy,
}

impl DockerPolicy {
    pub fn allows_job(&self, job_name: &str) -> bool {
        logical_name_allowed(&self.allowed_job_names, job_name)
    }

    pub fn allows_container(&self, container_name: &str) -> bool {
        logical_name_allowed(&self.allowed_container_names, container_name)
    }

    pub fn logic_container(&self, container_name: &str) -> Option<&LogicContainerPolicy> {
        self.logic_containers.get(container_name)
    }
}

/// Per logic-container capability policy. This is deliberately named and
/// stable; the actual Docker ID is resolved by GitRun at runtime and is not
/// a durable security identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicContainerPolicy {
    pub connect: bool,
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub melt: bool,
    pub mountable: bool,
    /// Logical names this source container may be melted into. The special
    /// value "runner" means the requesting runner's container. Empty means
    /// no target is permitted; * explicitly allows every configured target.
    pub allowed_melt_targets: Vec<String>,
}

impl LogicContainerPolicy {
    pub fn allows_operation(&self, operation: GitRunOperation) -> bool {
        match operation {
            GitRunOperation::Connect => self.connect,
            GitRunOperation::Read => self.read,
            GitRunOperation::Write => self.write,
            GitRunOperation::Execute => self.execute,
            GitRunOperation::Melt => self.melt,
            _ => false,
        }
    }

    pub fn allows_melt_target(&self, target: &str) -> bool {
        logical_name_allowed(&self.allowed_melt_targets, target)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPolicy {
    /// Explicit deny-by-default mount policy. A rule may then permit a host
    /// file, directory, filesystem, volume, or other source according to the
    /// resource identity resolved by GitRun.
    pub enabled: bool,
    pub rules: Vec<MountRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRule {
    pub source: String,
    /// When true the source may be mounted recursively; when false the
    /// enforcement layer should require an exact source match.
    pub recursive: bool,
    pub read_only: bool,
    pub allow: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultPolicy {
    pub read: bool,
    pub write: bool,
    pub exists: bool,
    pub delete: bool,
    /// Metadata-only listing; values are never returned by this permission.
    pub list_metadata: bool,
    pub allowed_names: Vec<String>,
}

impl VaultPolicy {
    pub fn allows_name(&self, name: &str) -> bool {
        logical_name_allowed(&self.allowed_names, name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedStoragePolicy {
    pub enabled: bool,
    pub allow_files: bool,
    pub allow_logs: bool,
    pub max_file_size_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterPolicy {
    pub enabled: bool,
    pub allow_workflow: bool,
    pub allow_permanent: bool,
    pub allowed_entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRepositorySettings {
    pub api_policy: PolicyMatrix,
    pub docker: DockerPolicy,
    pub vault: VaultPolicy,
    pub storage: SharedStoragePolicy,
    pub register: RegisterPolicy,
}

fn logical_name_allowed(allowed: &[String], value: &str) -> bool {
    allowed
        .iter()
        .any(|candidate| candidate == "*" || candidate == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_policy::GitRunOperation;

    #[test]
    fn default_settings_are_fail_closed() {
        let settings = GitRunSettings::default();
        assert!(!settings
            .global
            .allows(GitRunApi::GitDockRun, GitRunOperation::Connect));
        assert!(
            !settings
                .effective_for_repository("owner/repo")
                .docker
                .direct_socket_enabled
        );
    }

    #[test]
    fn repository_cannot_reenable_globally_disabled_api() {
        let mut settings = GitRunSettings::default();
        settings.repositories.insert(
            "owner/repo".into(),
            RepositorySettings {
                api_overrides: BTreeMap::from([(
                    GitRunApi::GitDockRun,
                    ApiPolicyOverride {
                        enabled: Some(true),
                        allowed_operations: Some([GitRunOperation::Connect].into_iter().collect()),
                    },
                )]),
                ..RepositorySettings::default()
            },
        );

        let effective = settings.effective_for_repository("owner/repo");
        assert!(!effective
            .api_policy
            .allows(GitRunApi::GitDockRun, GitRunOperation::Connect));
    }

    #[test]
    fn repository_can_narrow_enabled_api() {
        let mut settings = GitRunSettings::default();
        settings.global.set(
            GitRunApi::GitDockRun,
            ApiPolicy::enabled_with([
                GitRunOperation::Connect,
                GitRunOperation::Disconnect,
                GitRunOperation::Melt,
            ]),
        );
        settings.repositories.insert(
            "owner/repo".into(),
            RepositorySettings {
                api_overrides: BTreeMap::from([(
                    GitRunApi::GitDockRun,
                    ApiPolicyOverride {
                        enabled: Some(true),
                        allowed_operations: Some(
                            [GitRunOperation::Connect, GitRunOperation::Disconnect]
                                .into_iter()
                                .collect(),
                        ),
                    },
                )]),
                ..RepositorySettings::default()
            },
        );

        let effective = settings.effective_for_repository("owner/repo");
        assert!(effective
            .api_policy
            .allows(GitRunApi::GitDockRun, GitRunOperation::Connect));
        assert!(effective
            .api_policy
            .allows(GitRunApi::GitDockRun, GitRunOperation::Disconnect));
        assert!(!effective
            .api_policy
            .allows(GitRunApi::GitDockRun, GitRunOperation::Melt));
    }

    #[test]
    fn container_policy_can_deny_melt_without_disabling_dock() {
        let mut docker = DockerPolicy::default();
        docker.logic_containers.insert(
            "cache".into(),
            LogicContainerPolicy {
                melt: false,
                ..LogicContainerPolicy::default()
            },
        );
        assert!(!docker.logic_container("cache").unwrap().melt);
    }

    #[test]
    fn melt_target_is_allowlisted_explicitly() {
        let policy = LogicContainerPolicy {
            melt: true,
            allowed_melt_targets: vec!["runner".into(), "build".into()],
            ..LogicContainerPolicy::default()
        };
        assert!(policy.allows_melt_target("runner"));
        assert!(policy.allows_melt_target("build"));
        assert!(!policy.allows_melt_target("production"));
    }

    #[test]
    fn settings_save_is_round_trippable() {
        let dir = std::env::temp_dir().join(format!("gitrun-settings-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join(SETTINGS_FILE_NAME);

        let settings = GitRunSettings::default();
        settings.save(&path).unwrap();
        let loaded = GitRunSettings::load(&path).unwrap();
        assert_eq!(loaded, settings);

        let _ = fs::remove_dir_all(&dir);
    }
}
