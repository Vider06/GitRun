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
}

/// Closed set of operation verbs understood by the GitRun APIs.
///
/// The same operation verb may be used by multiple APIs, but it is always
/// interpreted in the context of the enclosing GitRunApi.
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
    Permanent,
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
            Self::Permanent => "permanent",
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
        self.get(api).allows(operation)
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
    fn api_policy_can_limit_operations() {
        let policy = ApiPolicy::enabled_with([GitRunOperation::Connect, GitRunOperation::Disconnect]);
        assert!(policy.allows(GitRunOperation::Connect));
        assert!(!policy.allows(GitRunOperation::Melt));
    }

    #[test]
    fn intersection_never_grants_capability() {
        let broad = ApiPolicy::enabled_with([
            GitRunOperation::Connect,
            GitRunOperation::Disconnect,
            GitRunOperation::Read,
            GitRunOperation::Melt,
        ]);
        let narrow = ApiPolicy::enabled_with([GitRunOperation::Connect, GitRunOperation::Read]);
        let effective = broad.intersection(&narrow);

        assert!(effective.allows(GitRunOperation::Connect));
        assert!(effective.allows(GitRunOperation::Read));
        assert!(!effective.allows(GitRunOperation::Disconnect));
        assert!(!effective.allows(GitRunOperation::Melt));
    }

    #[test]
    fn api_names_are_stable() {
        assert_eq!(GitRunApi::GitDockRun.as_str(), "GitDockRun");
        assert_eq!(GitRunApi::GitVaultRun.as_str(), "GitVaultRun");
        assert_eq!(GitRunOperation::Melt.as_str(), "melt");
    }
}
