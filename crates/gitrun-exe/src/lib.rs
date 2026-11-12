//! Internal execution engine for GitRun.
//!
//! This crate is intentionally not a workflow-facing command API.
//! Workflow invocations are modeled as explicit GitRun APIs, authenticated
//! and authorized by GSR before they reach this layer. There is deliberately
//! no variant for arbitrary shell/Docker commands.

use gitrun_core::api_policy::{GitRunApi, GitRunOperation};
#[cfg(not(any(kani, verus)))]
use serde::{Deserialize, Serialize};
#[cfg(not(any(kani, verus)))]
use std::collections::BTreeMap;
#[cfg(not(any(kani, verus)))]
use thiserror::Error;

#[cfg(not(any(kani, verus)))]
pub mod ipc;
#[cfg(not(any(kani, verus)))]
pub mod protocol;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(not(any(kani, verus)))]
pub struct ApiInvocation {
    pub api: GitRunApi,
    pub operation: GitRunOperation,
    pub repository: String,
    pub workflow: String,
    pub run_id: Option<u64>,
    pub job: String,
    pub runner: String,
    pub resource: Option<String>,
    pub arguments: BTreeMap<String, String>,
}

#[cfg(not(any(kani, verus)))]
impl ApiInvocation {
    pub fn into_authorized(self, request_id: String) -> AuthorizedOperation {
        AuthorizedOperation {
            request_id,
            api: self.api,
            operation: self.operation,
            repository: self.repository,
            workflow: self.workflow,
            run_id: self.run_id,
            job: self.job,
            resource: self.resource,
            arguments: self.arguments,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(not(any(kani, verus)))]
pub struct AuthorizedOperation {
    pub request_id: String,
    pub api: GitRunApi,
    pub operation: GitRunOperation,
    pub repository: String,
    pub workflow: String,
    pub run_id: Option<u64>,
    pub job: String,
    /// Logical resource identity, never trusted as a raw Docker command.
    pub resource: Option<String>,
    /// Parsed API arguments. GSR is responsible for schema validation before
    /// creating this request; handlers must still validate resource-specific
    /// invariants before doing privileged work.
    pub arguments: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(not(any(kani, verus)))]
pub enum ExecutionEvent {
    Started,
    Stdout(String),
    Stderr(String),
    Finished { exit_code: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(not(any(kani, verus)))]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(any(kani, verus)), derive(Error))]
pub enum ExecutionError {
    #[cfg_attr(
        not(any(kani, verus)),
        error("execution backend does not implement {api:?}/{operation:?}")
    )]
    Unsupported {
        api: GitRunApi,
        operation: GitRunOperation,
    },
    #[cfg_attr(not(any(kani, verus)), error("execution failed: {0}"))]
    Failed(String),
}

/// Internal boundary used by GSR-approved handlers.
///
/// Implementations should be composed from explicit API handlers
/// (GitVaultRun, GitDockRun, ...) rather than a generic shell runner.
#[cfg(not(any(kani, verus)))]
pub trait ExecutionBackend: Send + Sync {
    fn execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError>;

    /// Streaming adapter used by the IPC server. Backends that can stream
    /// process output should override this method; the default keeps older
    /// handlers correct by emitting the buffered result as events.
    fn execute_stream(
        &self,
        request: &AuthorizedOperation,
        sink: &mut dyn FnMut(ExecutionEvent),
    ) -> Result<i32, ExecutionError> {
        sink(ExecutionEvent::Started);
        let result = match self.execute(request) {
            Ok(result) => result,
            Err(error) => {
                sink(ExecutionEvent::Stderr(error.to_string()));
                sink(ExecutionEvent::Finished { exit_code: 1 });
                return Err(error);
            }
        };
        if !result.stdout.is_empty() {
            sink(ExecutionEvent::Stdout(result.stdout.clone()));
        }
        if !result.stderr.is_empty() {
            sink(ExecutionEvent::Stderr(result.stderr.clone()));
        }
        sink(ExecutionEvent::Finished {
            exit_code: result.exit_code,
        });
        Ok(result.exit_code)
    }
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
fn noop_backend_unsupported_error(api: GitRunApi, operation: GitRunOperation) -> ExecutionError {
    ExecutionError::Unsupported { api, operation }
}

/// Safe default backend used while the concrete handlers are introduced.
/// It guarantees that an accidentally unregistered operation cannot execute.
#[derive(Debug, Default, Clone, Copy)]
#[cfg(not(any(kani, verus)))]
pub struct NoopExecutionBackend;

#[cfg(not(any(kani, verus)))]
impl ExecutionBackend for NoopExecutionBackend {
    fn execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        Err(noop_backend_unsupported_error(
            request.api,
            request.operation,
        ))
    }
}

#[cfg(test)]
#[cfg(not(any(kani, verus)))]
mod tests {
    use super::*;

    fn request(api: GitRunApi, operation: GitRunOperation) -> AuthorizedOperation {
        AuthorizedOperation {
            request_id: "test-request".into(),
            api,
            operation,
            repository: "owner/repo".into(),
            workflow: "ci.yml".into(),
            run_id: Some(1),
            job: "build".into(),
            resource: None,
            arguments: BTreeMap::new(),
        }
    }

    #[test]
    fn default_stream_adapter_finishes_even_when_execution_fails() {
        let backend = NoopExecutionBackend;
        let request = request(GitRunApi::GitDockRun, GitRunOperation::Melt);
        let mut events = Vec::new();

        let result = backend.execute_stream(&request, &mut |event| events.push(event));

        assert!(matches!(result, Err(ExecutionError::Unsupported { .. })));
        assert!(matches!(events.first(), Some(ExecutionEvent::Started)));
        assert!(
            matches!(events.get(1), Some(ExecutionEvent::Stderr(message)) if message.contains("does not implement"))
        );
        assert!(matches!(
            events.last(),
            Some(ExecutionEvent::Finished { exit_code: 1 })
        ));
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

#[cfg(kani)]
mod kani_verification {
    use super::{noop_backend_unsupported_error, ExecutionError};
    use gitrun_core::api_policy::{GitRunApi, GitRunOperation};

    #[kani::proof]
    #[kani::unwind(21)]
    fn noop_backend_rejects_every_declared_api_operation_pair() {
        let apis = [
            GitRunApi::GitVaultRun,
            GitRunApi::GitDockRun,
            GitRunApi::GitSaveRun,
            GitRunApi::GitRegisterRun,
            GitRunApi::GitInstallRun,
            GitRunApi::GitReadRun,
            GitRunApi::GitWriteRun,
            GitRunApi::GitVerifyRun,
            GitRunApi::GitStatusRun,
            GitRunApi::GitDispatchRun,
        ];
        let operations = [
            GitRunOperation::Read,
            GitRunOperation::Write,
            GitRunOperation::Exists,
            GitRunOperation::Delete,
            GitRunOperation::List,
            GitRunOperation::Connect,
            GitRunOperation::Disconnect,
            GitRunOperation::Execute,
            GitRunOperation::Melt,
            GitRunOperation::Mount,
            GitRunOperation::Unmount,
            GitRunOperation::File,
            GitRunOperation::Logs,
            GitRunOperation::Register,
            GitRunOperation::Install,
            GitRunOperation::Remove,
            GitRunOperation::Update,
            GitRunOperation::Verify,
            GitRunOperation::Status,
            GitRunOperation::Plan,
        ];
        for api in apis {
            for operation in operations {
                let error = noop_backend_unsupported_error(api, operation);
                assert!(
                    matches!(error, ExecutionError::Unsupported { api: actual_api, operation: actual_operation }
                    if actual_api == api && actual_operation == operation)
                );
            }
        }
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::{noop_backend_unsupported_error, ExecutionError};
    use gitrun_core::api_policy::{GitRunApi, GitRunOperation};
    use vstd::prelude::*;

    verus! {
        proof fn noop_backend_rejects_every_declared_api_operation_pair(
            api: GitRunApi,
            operation: GitRunOperation,
        )
            ensures
                matches!(
                    noop_backend_unsupported_error(api, operation),
                    ExecutionError::Unsupported { api: actual_api, operation: actual_operation }
                        if actual_api == api && actual_operation == operation
                ),
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
