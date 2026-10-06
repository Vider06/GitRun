//! Internal execution engine for GitRun.
//!
//! This crate is intentionally not a workflow-facing command API.
//! Workflow invocations are modeled as explicit GitRun APIs, authenticated
//! and authorized by GSR before they reach this layer. There is deliberately
//! no variant for arbitrary shell/Docker commands.

use gitrun_core::{GitRunApi, GitRunOperation};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedOperation {
    pub request_id: String,
    pub api: GitRunApi,
    pub operation: GitRunOperation,
    pub repository: String,
    pub workflow: String,
    pub job: String,
    /// Logical resource identity, never trusted as a raw Docker command.
    pub resource: Option<String>,
    /// Parsed API arguments. GSR is responsible for schema validation before
    /// creating this request; handlers must still validate resource-specific
    /// invariants before doing privileged work.
    pub arguments: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("execution backend does not implement {api:?}/{operation:?}")]
    Unsupported {
        api: GitRunApi,
        operation: GitRunOperation,
    },
    #[error("execution failed: {0}")]
    Failed(String),
}

/// Internal boundary used by GSR-approved handlers.
///
/// Implementations should be composed from explicit API handlers
/// (GitVaultRun, GitDockRun, ...) rather than a generic shell runner.
pub trait ExecutionBackend: Send + Sync {
    fn execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError>;
}

/// Safe default backend used while the concrete handlers are introduced.
/// It guarantees that an accidentally unregistered operation cannot execute.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopExecutionBackend;

impl ExecutionBackend for NoopExecutionBackend {
    fn execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        Err(ExecutionError::Unsupported {
            api: request.api,
            operation: request.operation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(api: GitRunApi, operation: GitRunOperation) -> AuthorizedOperation {
        AuthorizedOperation {
            request_id: "test-request".into(),
            api,
            operation,
            repository: "owner/repo".into(),
            workflow: "ci.yml".into(),
            job: "build".into(),
            resource: None,
            arguments: BTreeMap::new(),
        }
    }

    #[test]
    fn noop_backend_never_executes_unknown_operation() {
        let backend = NoopExecutionBackend;
        let error = backend
            .execute(&request(GitRunApi::GitDockRun, GitRunOperation::Melt))
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Unsupported { .. }));
    }
}
