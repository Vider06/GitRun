//! Declarative policy model for the closed set of GitRun workflow APIs.
//!
//! These types describe intentional Git*Run operations. They are not a
//! shell/command API and deliberately contain no arbitrary-command operation.
//! GSR consumes this model to decide whether an invocation may reach GitRun's
//! internal execution engine.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Closed set of workflow-facing GitRun APIs.
///
/// Adding a new API is an explicit source-level change so the security
/// surface cannot grow accidentally through configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GitRunApi {
    GitVaultRun,
    GitDockRun,
    GitSaveRun,
    GitRegisterRun,
    GitInstallRun,
    GitReadRun,
    GitWriteRun,
    GitVerifyRun,
    GitStatusRun,
}

impl GitRunApi {
    pub const ALL: [Self; 9] = [
        Self::GitVaultRun,
        Self::GitDockRun,
        Self::GitSaveRun,
        Self::GitRegisterRun,
        Self::GitInstallRun,
        Self::GitReadRun,
        Self::GitWriteRun,
        Self::GitVerifyRun,
        Self::GitStatusRun,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitVaultRun => "GitVaultRun",
            Self::GitDockRun => "GitDockRun",
            Self::GitSaveRun => "GitSaveRun",
            Self::GitRegisterRun => "GitRegisterRun",
            Self::GitInstallRun => "GitInstallRun",
            Self::GitReadRun => "GitReadRun",
            Self::GitWriteRun => "GitWriteRun",
            Self::GitVerifyRun => "GitVerifyRun",
            Self::GitStatusRun => "GitStatusRun",
        }
    }

    /// Returns true only for an operation that is part of this API's
    /// explicitly defined contract. Policy configuration cannot create a new
    /// API/operation pairing by accident.
    pub const fn supports_operation(self, operation: GitRunOperation) -> bool {
        match self {
            Self::GitVaultRun => matches!(
                operation,
                GitRunOperation::Read
                    | GitRunOperation::Write
                    | GitRunOperation::Exists
                    | GitRunOperation::Delete
                    | GitRunOperation::List
            ),
            Self::GitDockRun => matches!(
                operation,
                GitRunOperation::Connect
                    | GitRunOperation::Disconnect
                    | GitRunOperation::Read
                    | GitRunOperation::Write
                    | GitRunOperation::Execute
                    | GitRunOperation::Melt
            ),
            Self::GitSaveRun => matches!(operation, GitRunOperation::File | GitRunOperation::Logs),
            Self::GitRegisterRun => matches!(operation, GitRunOperation::Register),
            Self::GitInstallRun => matches!(
                operation,
                GitRunOperation::Install | GitRunOperation::Remove | GitRunOperation::Update
            ),
            Self::GitReadRun => matches!(operation, GitRunOperation::Read),
            Self::GitWriteRun => matches!(operation, GitRunOperation::Write),
            Self::GitVerifyRun => matches!(operation, GitRunOperation::Verify),
            Self::GitStatusRun => matches!(operation, GitRunOperation::Status),
        }
    }
}

/// Validates the argument shape of one public Git*Run operation.
///
/// The internal request representation uses a string map, but the public
/// surface is still closed: unknown keys and missing required values are
/// rejected here before a request can be authorized or executed.
pub fn validate_arguments(
    api: GitRunApi,
    operation: GitRunOperation,
    arguments: &BTreeMap<String, String>,
) -> Result<(), String> {
    if !api.supports_operation(operation) {
        return Err("operation is not part of this API".into());
    }

    let required: &[&str] = match (api, operation) {
        (GitRunApi::GitVaultRun, GitRunOperation::Read)
        | (GitRunApi::GitVaultRun, GitRunOperation::Write)
        | (GitRunApi::GitVaultRun, GitRunOperation::Exists)
        | (GitRunApi::GitVaultRun, GitRunOperation::Delete) => &["name"],
        (GitRunApi::GitVaultRun, GitRunOperation::List) => &[],
        (GitRunApi::GitDockRun, GitRunOperation::Connect)
        | (GitRunApi::GitDockRun, GitRunOperation::Disconnect) => &["job"],
        (GitRunApi::GitDockRun, GitRunOperation::Read)
        | (GitRunApi::GitDockRun, GitRunOperation::Write)
        | (GitRunApi::GitDockRun, GitRunOperation::Execute)
        | (GitRunApi::GitDockRun, GitRunOperation::Melt) => &[],
        (GitRunApi::GitSaveRun, GitRunOperation::File) => &["path"],
        (GitRunApi::GitSaveRun, GitRunOperation::Logs) => &["namefile"],
        (GitRunApi::GitRegisterRun, GitRunOperation::Register) => &["name", "entry", "permanent"],
        (GitRunApi::GitInstallRun, GitRunOperation::Install)
        | (GitRunApi::GitInstallRun, GitRunOperation::Remove) => &["package"],
        (GitRunApi::GitInstallRun, GitRunOperation::Update) => &[],
        (GitRunApi::GitReadRun, GitRunOperation::Read) => &["path"],
        (GitRunApi::GitWriteRun, GitRunOperation::Write) => &["path", "value"],
        (GitRunApi::GitVerifyRun, GitRunOperation::Verify) => &["path"],
        (GitRunApi::GitStatusRun, GitRunOperation::Status) => &[],
        _ => &[],
    };

    for key in required {
        if !arguments
            .get(*key)
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(format!("missing argument: {key}"));
        }
    }

    let allowed: &[&str] = match (api, operation) {
        (GitRunApi::GitVaultRun, GitRunOperation::Read)
        | (GitRunApi::GitVaultRun, GitRunOperation::Exists)
        | (GitRunApi::GitVaultRun, GitRunOperation::Delete) => &["name"],
        (GitRunApi::GitVaultRun, GitRunOperation::Write) => &["name", "value"],
        (GitRunApi::GitVaultRun, GitRunOperation::List) => &[],
        (GitRunApi::GitDockRun, GitRunOperation::Connect)
        | (GitRunApi::GitDockRun, GitRunOperation::Disconnect) => &["job"],
        (GitRunApi::GitDockRun, GitRunOperation::Read) => &["path"],
        (GitRunApi::GitDockRun, GitRunOperation::Write) => &["path", "value"],
        (GitRunApi::GitDockRun, GitRunOperation::Execute) => &["command"],
        (GitRunApi::GitDockRun, GitRunOperation::Melt) => &["target"],
        (GitRunApi::GitSaveRun, GitRunOperation::File) => &["path"],
        (GitRunApi::GitSaveRun, GitRunOperation::Logs) => &["namefile"],
        (GitRunApi::GitRegisterRun, GitRunOperation::Register) => &["name", "entry", "permanent"],
        (GitRunApi::GitInstallRun, GitRunOperation::Install) => &["package", "version"],
        (GitRunApi::GitInstallRun, GitRunOperation::Remove) => &["package"],
        (GitRunApi::GitInstallRun, GitRunOperation::Update) => &["package"],
        (GitRunApi::GitReadRun, GitRunOperation::Read)
        | (GitRunApi::GitVerifyRun, GitRunOperation::Verify) => &["path"],
        (GitRunApi::GitWriteRun, GitRunOperation::Write) => &["path", "value"],
        (GitRunApi::GitStatusRun, GitRunOperation::Status) => &[],
        _ => &[],
    };

    if let Some(unknown) = arguments
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        return Err(format!("unknown argument: {unknown}"));
    }

    Ok(())
}
/// Closed set of operation verbs understood by the GitRun APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GitRunOperation {
    Read,
    Write,
    Exists,
    Delete,
    List,
    Connect,
    Disconnect,
    Execute,
    Melt,
    File,
    Logs,
    Register,
    Install,
    Remove,
    Update,
    Verify,
    Status,
}

impl GitRunOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exists => "exists",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Execute => "execute",
            Self::Melt => "melt",
            Self::File => "file",
            Self::Logs => "logs",
            Self::Register => "register",
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Update => "update",
            Self::Verify => "verify",
            Self::Status => "status",
        }
    }
}

/// Policy for one API. An API may be disabled completely or restricted to an
/// explicit subset of its known operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPolicy {
    pub enabled: bool,
    pub allowed_operations: BTreeSet<GitRunOperation>,
}

impl ApiPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allowed_operations: BTreeSet::new(),
        }
    }

    pub fn enabled_with(allowed_operations: impl IntoIterator<Item = GitRunOperation>) -> Self {
        Self {
            enabled: true,
            allowed_operations: allowed_operations.into_iter().collect(),
        }
    }

    pub fn allows(&self, operation: GitRunOperation) -> bool {
        self.enabled && self.allowed_operations.contains(&operation)
    }

    /// Intersects capabilities so a narrower scope can only remove power,
    /// never grant a capability that the wider scope did not allow.
    pub fn intersection(&self, narrower: &Self) -> Self {
        Self {
            enabled: self.enabled && narrower.enabled,
            allowed_operations: self
                .allowed_operations
                .intersection(&narrower.allowed_operations)
                .copied()
                .collect(),
        }
    }
}

impl Default for ApiPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Complete API policy matrix. Missing entries are intentionally treated as
/// disabled by allows, which makes configuration fail closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyMatrix {
    pub apis: BTreeMap<GitRunApi, ApiPolicy>,
}

impl PolicyMatrix {
    pub fn secure_default() -> Self {
        Self::default()
    }

    pub fn set(&mut self, api: GitRunApi, policy: ApiPolicy) {
        self.apis.insert(api, policy);
    }

    pub fn get(&self, api: GitRunApi) -> ApiPolicy {
        self.apis.get(&api).cloned().unwrap_or_default()
    }

    pub fn allows(&self, api: GitRunApi, operation: GitRunOperation) -> bool {
        api.supports_operation(operation) && self.get(api).allows(operation)
    }

    /// Compute the capabilities left after applying a narrower policy
    /// matrix. APIs absent from either side become disabled.
    pub fn intersection(&self, narrower: &Self) -> Self {
        let apis = GitRunApi::ALL
            .into_iter()
            .map(|api| (api, self.get(api).intersection(&narrower.get(api))))
            .collect();
        Self { apis }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_api_entries_cannot_be_enabled_by_default() {
        let matrix = PolicyMatrix::default();
        assert!(!matrix.allows(GitRunApi::GitDockRun, GitRunOperation::Connect));
    }

    #[test]
    fn invalid_api_operation_pair_is_always_denied() {
        let mut matrix = PolicyMatrix::default();
        matrix.set(
            GitRunApi::GitStatusRun,
            ApiPolicy::enabled_with([GitRunOperation::Write]),
        );

        assert!(!matrix.allows(GitRunApi::GitStatusRun, GitRunOperation::Write));
        assert!(GitRunApi::GitDockRun.supports_operation(GitRunOperation::Write));
    }

    #[test]
    fn api_policy_can_limit_operations() {
        let policy =
            ApiPolicy::enabled_with([GitRunOperation::Connect, GitRunOperation::Disconnect]);
        assert!(policy.allows(GitRunOperation::Connect));
        assert!(!policy.allows(GitRunOperation::Melt));
    }

    #[test]
    fn intersection_never_grants_capability() {
        let broad = ApiPolicy::enabled_with([
            GitRunOperation::Connect,
            GitRunOperation::Disconnect,
            GitRunOperation::Melt,
        ]);
        let narrow = ApiPolicy::enabled_with([GitRunOperation::Connect, GitRunOperation::Read]);
        let effective = broad.intersection(&narrow);

        assert!(effective.allows(GitRunOperation::Connect));
        assert!(!effective.allows(GitRunOperation::Disconnect));
        assert!(!effective.allows(GitRunOperation::Melt));
    }

    #[test]
    fn argument_contract_rejects_unknown_arguments() {
        let args = BTreeMap::from([
            ("job".into(), "cache".into()),
            ("socket".into(), "docker.sock".into()),
        ]);
        assert_eq!(
            validate_arguments(GitRunApi::GitDockRun, GitRunOperation::Connect, &args),
            Err("unknown argument: socket".into())
        );
    }

    #[test]
    fn argument_contract_requires_vault_name() {
        assert_eq!(
            validate_arguments(
                GitRunApi::GitVaultRun,
                GitRunOperation::Read,
                &BTreeMap::new(),
            ),
            Err("missing argument: name".into())
        );
    }

    #[test]
    fn argument_contract_allows_optional_install_version() {
        let args = BTreeMap::from([
            ("package".into(), "ImageMagick".into()),
            ("version".into(), "1.0.0".into()),
        ]);
        assert!(
            validate_arguments(GitRunApi::GitInstallRun, GitRunOperation::Install, &args,).is_ok()
        );
    }

    #[test]
    fn argument_contract_melt_target_is_optional() {
        assert!(validate_arguments(
            GitRunApi::GitDockRun,
            GitRunOperation::Melt,
            &BTreeMap::new(),
        )
        .is_ok());
        let args = BTreeMap::from([("target".into(), "build".into())]);
        assert!(validate_arguments(GitRunApi::GitDockRun, GitRunOperation::Melt, &args,).is_ok());
    }
    #[test]
    fn api_names_are_stable() {
        assert_eq!(GitRunApi::GitDockRun.as_str(), "GitDockRun");
        assert_eq!(GitRunApi::GitVaultRun.as_str(), "GitVaultRun");
        assert_eq!(GitRunOperation::Melt.as_str(), "melt");
    }
    #[test]
    fn install_accepts_package_with_optional_version() {
        assert!(validate_arguments(
            GitRunApi::GitInstallRun,
            GitRunOperation::Install,
            &BTreeMap::from([
                ("package".into(), "ImageMagick".into()),
                ("version".into(), "1.0.0".into()),
            ]),
        )
        .is_ok());
    }

    #[test]
    fn register_requires_permanent_modifier_and_rejects_unknown_keys() {
        assert!(validate_arguments(
            GitRunApi::GitRegisterRun,
            GitRunOperation::Register,
            &BTreeMap::from([
                ("name".into(), "tool".into()),
                ("entry".into(), "/opt/tool".into()),
                ("permanent".into(), "false".into()),
            ]),
        )
        .is_ok());

        let result = validate_arguments(
            GitRunApi::GitRegisterRun,
            GitRunOperation::Register,
            &BTreeMap::from([
                ("name".into(), "tool".into()),
                ("entry".into(), "/opt/tool".into()),
                ("permanent".into(), "false".into()),
                ("unexpected".into(), "value".into()),
            ]),
        );
        assert!(matches!(result, Err(error) if error.contains("unknown argument")));
    }
}
