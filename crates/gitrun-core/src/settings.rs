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
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static SETTINGS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
use thiserror::Error;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
const SETTINGS_FILE_NAME: &str = "gitrun-settings.json";

/// Cross-process exclusion for security-policy readers performing a privileged
/// operation and writers replacing the policy file. Atomic rename protects
/// readers from partial JSON, but by itself cannot prevent a policy change
/// between authorization and the associated privileged side effect.
pub struct SettingsPolicyLock {
    file: Option<fs::File>,
}

impl SettingsPolicyLock {
    /// Lock the stable sidecar file associated with a settings path.
    ///
    /// Every supported writer uses this same lock through `GitRunSettings::save`.
    /// Security-sensitive operations may hold it from policy load until their
    /// side effect has completed. Direct edits that bypass GitRun's writer API
    /// remain administrative operations outside this advisory-lock contract.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = fs::symlink_metadata(parent)?;
                if !metadata.is_dir()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o022 != 0
                {
                    return Err(SettingsError::Io(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "GitRun settings directory must be a real directory owned by the current service identity and not group/world writable",
                    )));
                }
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

            let file_name = path.file_name().ok_or_else(|| {
                SettingsError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "GitRun settings path must name a file",
                ))
            })?;
            let lock_path = path.with_file_name(format!("{}.lock", file_name.to_string_lossy()));
            let mut options = fs::OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
            let file = options.open(&lock_path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(SettingsError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GitRun settings lock must be a single-link regular file owned by the current service identity",
                )));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
                return Err(SettingsError::Io(io::Error::last_os_error()));
            }
            Ok(Self { file: Some(file) })
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self { file: None })
        }
    }
}

impl Drop for SettingsPolicyLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = &self.file {
            let _ = unsafe {
                libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_UN)
            };
        }
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("unable to read settings: {0}")]
    Io(#[from] std::io::Error),
    #[error("unable to decode settings: {0}")]
    Decode(#[source] serde_json::Error),
    #[error(
        "unsupported settings schema version {found}; this build supports version {supported}"
    )]
    UnsupportedSchema { found: u32, supported: u32 },
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

    /// Lints global and repository policy without changing it. Repository
    /// policies are intentionally interpreted through the same intersection
    /// logic used at runtime, so the report describes the effective surface.
    pub fn lint(&self) -> Vec<crate::api_policy::PolicyLintIssue> {
        let mut issues = self.global.lint();
        for (repository, settings) in &self.repositories {
            let effective = self.effective_for_repository(repository);
            for issue in effective.api_policy.lint() {
                issues.push(crate::api_policy::PolicyLintIssue {
                    path: format!("{repository}.{}", issue.path),
                    message: issue.message,
                });
            }
            if settings
                .docker
                .allowed_container_names
                .contains(&"*".to_owned())
                && settings.docker.logic_containers.is_empty()
            {
                issues.push(crate::api_policy::PolicyLintIssue {
                    path: format!("{repository}.docker.allowed_container_names"),
                    message: "wildcard container access is enabled without any named logic-container policy".into(),
                });
            }
        }
        issues
    }

    pub fn path_for_state_dir(state_dir: impl AsRef<Path>) -> PathBuf {
        state_dir.as_ref().join(SETTINGS_FILE_NAME)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let raw = fs::read_to_string(path)?;
        let settings: Self = serde_json::from_str(&raw).map_err(SettingsError::Decode)?;
        settings.validate_schema_version()?;
        Ok(settings)
    }

    fn validate_schema_version(&self) -> Result<(), SettingsError> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(SettingsError::UnsupportedSchema {
                found: self.schema_version,
                supported: SETTINGS_SCHEMA_VERSION,
            });
        }
        Ok(())
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
        self.validate_schema_version()?;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Hold the same cross-process lock that MOUNT holds around its policy
        // read and privileged namespace transition. Other official writers
        // cannot replace policy until that transition has reached a terminal
        // state.
        let _policy_lock = SettingsPolicyLock::acquire(path)?;

        let raw = serde_json::to_vec_pretty(self).map_err(SettingsError::Encode)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!(
            "json.{}-{timestamp:032x}-{sequence:016x}.tmp",
            std::process::id()
        ));

        let mut temp_created = false;
        let write_result = (|| -> Result<(), SettingsError> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            temp_created = true;
            file.write_all(&raw)?;
            file.sync_all()?;
            drop(file);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
            }

            fs::rename(&temp, path)?;

            // Persist the directory entry as well as the file contents so a
            // successful save has the strongest practical crash durability.
            #[cfg(unix)]
            {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();

        if write_result.is_err() && temp_created {
            let _ = fs::remove_file(&temp);
        }
        write_result
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
#[serde(default)]
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
    /// Whether GitDockRun may mount an explicitly approved host source into
    /// this logical container. The GSR gate checks this per-container capability.
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
            GitRunOperation::Mount => self.mountable,
            // Unmount stays available for an existing, correctly-bound mount
            // even if an administrator has since disabled new mounts.
            GitRunOperation::Unmount => true,
            _ => false,
        }
    }

    pub fn allows_melt_target(&self, target: &str) -> bool {
        logical_name_allowed(&self.allowed_melt_targets, target)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPolicy {
    /// Explicit deny-by-default allowlist of host source paths for GitDockRun.
    pub enabled: bool,
    pub rules: Vec<MountRule>,
}

impl MountPolicy {
    /// Resolve an approved source and return its canonical host path.
    ///
    /// Explicit deny rules override every allow rule. Among matching allow
    /// rules, the most-specific source governs the read-only requirement.
    /// Source and rule roots are canonicalized so symlinks cannot grant access
    /// to a path outside the approved tree.
    pub fn authorize_source(
        &self,
        source: &Path,
        requested_read_only: bool,
    ) -> Result<PathBuf, String> {
        if !self.enabled {
            return Err("GitDockRun host mounts are disabled for this repository".into());
        }
        if !source.is_absolute()
            || source.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("mount source must be a clean absolute path".into());
        }
        let canonical_source = fs::canonicalize(source)
            .map_err(|error| format!("cannot resolve mount source: {error}"))?;
        let mut matches: Vec<(&MountRule, PathBuf)> = Vec::new();

        for rule in &self.rules {
            let configured_source = Path::new(&rule.source);
            if !configured_source.is_absolute()
                || configured_source.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err("mount rule source must be a clean absolute path".into());
            }
            let rule_source = configured_source
                .canonicalize()
                .map_err(|error| format!("cannot resolve configured mount rule source: {error}"))?;
            let exact = canonical_source == rule_source;
            let descendant = rule.recursive
                && canonical_source != rule_source
                && canonical_source.starts_with(&rule_source);
            if exact || descendant {
                if !rule.allow {
                    return Err(format!(
                        "mount source is denied by an explicit rule for {}",
                        rule_source.display()
                    ));
                }
                matches.push((rule, rule_source));
            }
        }

        let Some(max_specificity) = matches
            .iter()
            .map(|(_, root)| root.components().count())
            .max()
        else {
            return Err("mount source does not match an allowed host-path rule".into());
        };
        // If multiple allow rules canonicalize to the same most-specific
        // root, combine their restrictions conservatively: any read-only rule
        // at that specificity prevents a writable mount.
        let most_specific_requires_read_only = matches
            .iter()
            .any(|(rule, root)| root.components().count() == max_specificity && rule.read_only);
        if most_specific_requires_read_only && !requested_read_only {
            return Err("most-specific mount rules permit read-only access only".into());
        }
        Ok(canonical_source)
    }
}

/// A source-path rule. Recursive permits matching descendants of source;
/// it does not request recursive kernel bind-mount propagation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRule {
    pub source: String,
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

impl RegisterPolicy {
    /// A registration entry must match an exact allowlist value; "*" is an
    /// explicit opt-in for every entry that passes the execution backend's
    /// independent path validation.
    pub fn allows_entry(&self, entry: &str) -> bool {
        logical_name_allowed(&self.allowed_entries, entry)
    }
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
    fn mount_policy_is_disabled_by_default() {
        let policy = MountPolicy::default();
        assert!(policy.authorize_source(Path::new("/tmp"), true).is_err());
    }

    #[test]
    fn mount_policy_matches_recursive_paths_and_enforces_read_only() {
        let dir = std::env::temp_dir().join(format!("gitrun-mount-policy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/file.txt"), "safe").unwrap();
        let policy = MountPolicy {
            enabled: true,
            rules: vec![MountRule {
                source: dir.to_string_lossy().into_owned(),
                recursive: true,
                read_only: true,
                allow: true,
            }],
        };
        assert_eq!(
            policy
                .authorize_source(&dir.join("nested/file.txt"), true)
                .unwrap(),
            fs::canonicalize(dir.join("nested/file.txt")).unwrap()
        );
        assert!(policy
            .authorize_source(&dir.join("nested/file.txt"), false)
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn equally_specific_mount_rules_combine_read_only_restrictions() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-mount-tied-rules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("source.txt"), "payload").unwrap();
        let rule_source = dir.to_string_lossy().into_owned();
        let policy = MountPolicy {
            enabled: true,
            rules: vec![
                MountRule {
                    source: rule_source.clone(),
                    recursive: true,
                    read_only: false,
                    allow: true,
                },
                MountRule {
                    source: rule_source,
                    recursive: true,
                    read_only: true,
                    allow: true,
                },
            ],
        };
        assert!(policy
            .authorize_source(&dir.join("source.txt"), false)
            .is_err());
        assert!(policy
            .authorize_source(&dir.join("source.txt"), true)
            .is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_mount_deny_overrides_parent_allow() {
        let dir = std::env::temp_dir().join(format!("gitrun-mount-deny-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("private")).unwrap();
        fs::write(dir.join("private/file.txt"), "secret").unwrap();
        let policy = MountPolicy {
            enabled: true,
            rules: vec![
                MountRule {
                    source: dir.to_string_lossy().into_owned(),
                    recursive: true,
                    read_only: true,
                    allow: true,
                },
                MountRule {
                    source: dir.join("private").to_string_lossy().into_owned(),
                    recursive: true,
                    read_only: true,
                    allow: false,
                },
            ],
        };
        assert!(policy
            .authorize_source(&dir.join("private/file.txt"), true)
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_policy_rejects_symlink_escape_outside_allowed_tree() {
        let base =
            std::env::temp_dir().join(format!("gitrun-mount-symlink-{}", std::process::id()));
        let allowed = base.join("allowed");
        let outside = base.join("outside");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        let policy = MountPolicy {
            enabled: true,
            rules: vec![MountRule {
                source: allowed.to_string_lossy().into_owned(),
                recursive: true,
                read_only: true,
                allow: true,
            }],
        };
        #[cfg(unix)]
        assert!(policy
            .authorize_source(&allowed.join("escape/secret.txt"), true)
            .is_err());
        let _ = fs::remove_dir_all(&base);
    }

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
    fn dashboard_partial_repository_settings_deserialize_with_safe_defaults() {
        // The dashboard creates a new repository policy before every policy
        // subsection exists. Missing subsections must use Rust defaults rather
        // than breaking the whole save with "missing field vault".
        let payload = serde_json::json!({
            "schema_version": SETTINGS_SCHEMA_VERSION,
            "global": PolicyMatrix::secure_default(),
            "repositories": {
                "owner/repo": {
                    "api_overrides": {},
                    "docker": DockerPolicy::default()
                }
            }
        });

        let settings: GitRunSettings = serde_json::from_value(payload)
            .expect("partial dashboard policy should deserialize safely");
        let repository = settings.repositories.get("owner/repo").unwrap();
        assert_eq!(repository.vault, VaultPolicy::default());
        assert_eq!(repository.storage, SharedStoragePolicy::default());
        assert_eq!(repository.register, RegisterPolicy::default());
        assert_eq!(repository.docker, DockerPolicy::default());
    }

    #[test]
    fn register_entries_are_deny_by_default_and_allow_exact_or_explicit_wildcard() {
        let empty = RegisterPolicy::default();
        assert!(!empty.allows_entry("/home/runner/tool"));

        let exact = RegisterPolicy {
            allowed_entries: vec!["/home/runner/tool".into()],
            ..RegisterPolicy::default()
        };
        assert!(exact.allows_entry("/home/runner/tool"));
        assert!(!exact.allows_entry("/home/runner/other"));

        let wildcard = RegisterPolicy {
            allowed_entries: vec!["*".into()],
            ..RegisterPolicy::default()
        };
        assert!(wildcard.allows_entry("/home/runner/any-safe-entry"));
    }

    #[test]
    fn settings_load_rejects_unsupported_schema_versions() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-settings-schema-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE_NAME);
        let mut value = serde_json::to_value(GitRunSettings::default()).unwrap();
        value["schema_version"] = serde_json::json!(SETTINGS_SCHEMA_VERSION + 1);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        assert!(matches!(
            GitRunSettings::load(&path),
            Err(SettingsError::UnsupportedSchema {
                found,
                supported: SETTINGS_SCHEMA_VERSION
            }) if found == SETTINGS_SCHEMA_VERSION + 1
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_save_rejects_unsupported_schema_versions() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-settings-save-schema-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE_NAME);
        let settings = GitRunSettings {
            schema_version: SETTINGS_SCHEMA_VERSION + 1,
            ..GitRunSettings::default()
        };

        assert!(matches!(
            settings.save(&path),
            Err(SettingsError::UnsupportedSchema {
                found,
                supported: SETTINGS_SCHEMA_VERSION
            }) if found == SETTINGS_SCHEMA_VERSION + 1
        ));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_configured_mount_rule_fails_closed() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-mount-invalid-rule-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.txt");
        fs::write(&source, "payload").unwrap();
        let policy = MountPolicy {
            enabled: true,
            rules: vec![
                MountRule {
                    source: dir.to_string_lossy().into_owned(),
                    recursive: true,
                    read_only: true,
                    allow: true,
                },
                MountRule {
                    source: dir.join("missing-deny-root").to_string_lossy().into_owned(),
                    recursive: true,
                    read_only: true,
                    allow: false,
                },
            ],
        };

        assert!(policy.authorize_source(&source, true).is_err());
        let _ = fs::remove_dir_all(&dir);
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

    #[cfg(unix)]
    #[test]
    fn policy_operation_lock_serializes_atomic_settings_replacements() {
        use std::sync::mpsc;
        use std::time::Duration;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gitrun-settings-policy-lock-{}-{nonce}",
            std::process::id()
        ));
        let path = dir.join(SETTINGS_FILE_NAME);
        GitRunSettings::default().save(&path).unwrap();

        let guard = SettingsPolicyLock::acquire(&path).unwrap();
        let worker_path = path.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            GitRunSettings::default().save(&worker_path).unwrap();
            finished_tx.send(()).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            finished_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "policy writer must wait while a privileged operation holds the policy lock"
        );
        drop(guard);
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("policy writer should proceed after the operation releases its lock");
        worker.join().unwrap();
        assert_eq!(GitRunSettings::load(&path).unwrap(), GitRunSettings::default());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let lock_path = path.with_file_name(format!(
                "{}.lock",
                path.file_name().unwrap().to_string_lossy()
            ));
            let target = dir.join("unrelated-lock-target");
            fs::write(&target, b"do not lock through a symlink").unwrap();
            fs::remove_file(&lock_path).unwrap();
            symlink(&target, &lock_path).unwrap();
            assert!(
                SettingsPolicyLock::acquire(&path).is_err(),
                "policy-lock symlink replacement must fail closed"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
