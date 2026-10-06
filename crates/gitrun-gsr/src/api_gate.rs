//! GSR authorization gate for workflow-facing GitRun APIs.
//!
//! This module performs authorization only. It does not execute anything.
//! A request must pass the API contract, effective repository policy, the
//! resource policy, and (for Dock execute) the existing GSR command policy
//! before it can be converted into an AuthorizedOperation for gitrun-exe.

use gitrun_core::{
    api_policy::{validate_arguments, GitRunApi, GitRunOperation},
    command_policy::{CommandPolicy, Decision},
    settings::EffectiveRepositorySettings,
    GitRunSettings,
};
use gitrun_exe::AuthorizedOperation;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ApiGateError {
    #[error("API {api:?} is disabled")]
    ApiDisabled { api: GitRunApi },
    #[error("operation {operation:?} is not available for API {api:?}")]
    OperationDisabled {
        api: GitRunApi,
        operation: GitRunOperation,
    },
    #[error("invalid {api:?}/{operation:?} request: {detail}")]
    InvalidArguments {
        api: GitRunApi,
        operation: GitRunOperation,
        detail: String,
    },
    #[error("resource is not allowed by repository policy: {resource}")]
    ResourceDenied { resource: String },
    #[error("repository policy denied API operation: {reason}")]
    PolicyDenied { reason: String },
    #[error("command denied by GSR command policy: {reason}")]
    CommandDenied { reason: String },
}

/// Identity already established by the GitRun/GSR transport layer.
///
/// The transport/authentication mechanism is intentionally not hidden inside
/// this authorization function: callers must provide a verified repository,
/// workflow, job and runner identity before asking GSR to authorize work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCaller {
    pub repository: String,
    pub workflow: String,
    pub job: String,
    pub runner: String,
}

impl VerifiedCaller {
    pub fn new(
        repository: impl Into<String>,
        workflow: impl Into<String>,
        job: impl Into<String>,
        runner: impl Into<String>,
    ) -> Self {
        Self {
            repository: repository.into(),
            workflow: workflow.into(),
            job: job.into(),
            runner: runner.into(),
        }
    }
}

/// Authorize one explicit Git*Run request and convert it to the internal
/// execution-engine request only after every gate has passed.
pub fn authorize(
    settings: &GitRunSettings,
    caller: &VerifiedCaller,
    api: GitRunApi,
    operation: GitRunOperation,
    resource: Option<&str>,
    arguments: BTreeMap<String, String>,
    command_policy: Option<&CommandPolicy>,
) -> Result<AuthorizedOperation, ApiGateError> {
    let effective = settings.effective_for_repository(&caller.repository);
    authorize_with_effective(
        &effective,
        caller,
        api,
        operation,
        resource,
        arguments,
        command_policy,
    )
}

pub fn authorize_with_effective(
    effective: &EffectiveRepositorySettings,
    caller: &VerifiedCaller,
    api: GitRunApi,
    operation: GitRunOperation,
    resource: Option<&str>,
    arguments: BTreeMap<String, String>,
    command_policy: Option<&CommandPolicy>,
) -> Result<AuthorizedOperation, ApiGateError> {
    let api_policy = effective.api_policy.get(api);
    if !api_policy.enabled {
        return Err(ApiGateError::ApiDisabled { api });
    }
    if !api.supports_operation(operation) || !api_policy.allows(operation) {
        return Err(ApiGateError::OperationDisabled { api, operation });
    }
    if let Err(detail) = validate_arguments(api, operation, &arguments) {
        return Err(ApiGateError::InvalidArguments {
            api,
            operation,
            detail,
        });
    }

    authorize_resource_scope(
        effective,
        caller,
        api,
        operation,
        resource,
        &arguments,
        command_policy,
    )?;

    Ok(AuthorizedOperation {
        request_id: request_id(caller, api, operation),
        api,
        operation,
        repository: caller.repository.clone(),
        workflow: caller.workflow.clone(),
        job: caller.job.clone(),
        resource: resource.map(str::to_owned),
        arguments,
    })
}

fn authorize_resource_scope(
    effective: &EffectiveRepositorySettings,
    caller: &VerifiedCaller,
    api: GitRunApi,
    operation: GitRunOperation,
    resource: Option<&str>,
    arguments: &BTreeMap<String, String>,
    command_policy: Option<&CommandPolicy>,
) -> Result<(), ApiGateError> {
    match api {
        GitRunApi::GitVaultRun => authorize_vault(effective, operation, arguments),
        GitRunApi::GitDockRun => authorize_dock(
            effective,
            caller,
            operation,
            resource,
            arguments,
            command_policy,
        ),
        GitRunApi::GitSaveRun => {
            if !effective.storage.enabled {
                return Err(ApiGateError::PolicyDenied {
                    reason: "shared storage is disabled".into(),
                });
            }
            match operation {
                GitRunOperation::File if effective.storage.allow_files => Ok(()),
                GitRunOperation::Logs if effective.storage.allow_logs => Ok(()),
                GitRunOperation::File | GitRunOperation::Logs => Err(ApiGateError::PolicyDenied {
                    reason: "requested shared-storage operation is disabled".into(),
                }),
                _ => Ok(()),
            }
        }
        GitRunApi::GitRegisterRun => {
            if !effective.register.enabled {
                return Err(ApiGateError::PolicyDenied {
                    reason: "resource registration is disabled".into(),
                });
            }
            let permanent = match arguments.get("permanent").map(String::as_str) {
                None => false,
                Some(value) if value.eq_ignore_ascii_case("true") => true,
                Some(value) if value.eq_ignore_ascii_case("false") => false,
                Some(_) => {
                    return Err(ApiGateError::InvalidArguments {
                        api: GitRunApi::GitRegisterRun,
                        operation: GitRunOperation::Register,
                        detail: "permanent must be true or false".into(),
                    })
                }
            };
            if permanent && effective.register.allow_permanent {
                return Ok(());
            }
            if !permanent && effective.register.allow_workflow {
                return Ok(());
            }
            Err(ApiGateError::PolicyDenied {
                reason: if permanent {
                    "permanent registration is disabled".into()
                } else {
                    "workflow registration is disabled".into()
                },
            })
        }
        _ => Ok(()),
    }
}

fn authorize_vault(
    effective: &EffectiveRepositorySettings,
    operation: GitRunOperation,
    arguments: &BTreeMap<String, String>,
) -> Result<(), ApiGateError> {
    let name = required_argument(GitRunApi::GitVaultRun, operation, arguments, "name")?;
    if !effective.vault.allows_name(name) {
        return Err(ApiGateError::ResourceDenied {
            resource: name.to_owned(),
        });
    }

    let allowed = match operation {
        GitRunOperation::Read => effective.vault.read,
        GitRunOperation::Write => effective.vault.write,
        GitRunOperation::Exists => effective.vault.exists,
        GitRunOperation::Delete => effective.vault.delete,
        GitRunOperation::List => effective.vault.list_metadata,
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(ApiGateError::PolicyDenied {
            reason: "vault operation is disabled".into(),
        })
    }
}

fn authorize_dock(
    effective: &EffectiveRepositorySettings,
    caller: &VerifiedCaller,
    operation: GitRunOperation,
    resource: Option<&str>,
    arguments: &BTreeMap<String, String>,
    command_policy: Option<&CommandPolicy>,
) -> Result<(), ApiGateError> {
    match operation {
        GitRunOperation::Connect | GitRunOperation::Disconnect => {
            let job_name = arguments
                .get("job")
                .map(String::as_str)
                .unwrap_or(&caller.job);
            if effective.docker.allows_job(job_name) {
                Ok(())
            } else {
                Err(ApiGateError::ResourceDenied {
                    resource: job_name.to_owned(),
                })
            }
        }
        GitRunOperation::Read
        | GitRunOperation::Write
        | GitRunOperation::Execute
        | GitRunOperation::Melt => {
            let container = resource
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| ApiGateError::InvalidArguments {
                    api: GitRunApi::GitDockRun,
                    operation,
                    detail: "missing docked resource".into(),
                })?;

            if !effective.docker.allows_container(container) {
                return Err(ApiGateError::ResourceDenied {
                    resource: container.to_owned(),
                });
            }

            let logic = effective.docker.logic_container(container).ok_or_else(|| {
                ApiGateError::ResourceDenied {
                    resource: container.to_owned(),
                }
            })?;

            if !logic.allows_operation(operation) {
                return Err(ApiGateError::PolicyDenied {
                    reason: format!("operation {} is disabled for logic container {container}", operation.as_str()),
                });
            }

            if operation == GitRunOperation::Melt {
                let target = arguments.get("target").map(String::as_str).unwrap_or("runner");
                if !logic.allows_melt_target(target) {
                    return Err(ApiGateError::PolicyDenied {
                        reason: format!("melt target {target} is not allowed for {container}"),
                    });
                }
            }

            if operation == GitRunOperation::Execute {
                let command = required_argument(
                    GitRunApi::GitDockRun,
                    GitRunOperation::Execute,
                    arguments,
                    "command",
                )?;
                let policy = command_policy.ok_or(ApiGateError::PolicyDenied {
                    reason: "GSR command policy is required for docked execute".into(),
                })?;
                if let Decision::Denied { reason } = policy.evaluate(command) {
                    return Err(ApiGateError::CommandDenied { reason });
                }
            }

            Ok(())
        }
        _ => Ok(()),
    }
}

fn required_argument<'a>(
    api: GitRunApi,
    operation: GitRunOperation,
    arguments: &'a BTreeMap<String, String>,
    name: &'static str,
) -> Result<&'a str, ApiGateError> {
    arguments
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiGateError::InvalidArguments {
            api,
            operation,
            detail: format!("missing argument: {name}"),
        })
}

fn request_id(
    caller: &VerifiedCaller,
    api: GitRunApi,
    operation: GitRunOperation,
) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        caller.repository, caller.workflow, caller.job, caller.runner, api.as_str(), operation.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitrun_core::{ApiPolicy, DockerPolicy, GitRunOperation, LogicContainerPolicy, RepositorySettings};

    fn settings_for_dock() -> GitRunSettings {
        let mut settings = GitRunSettings::default();
        let mut docker = DockerPolicy {
            allowed_job_names: vec!["cache".into()],
            allowed_container_names: vec!["cache".into()],
            logic_containers: Default::default(),
            ..DockerPolicy::default()
        };
        docker.logic_containers.insert(
            "cache".into(),
            LogicContainerPolicy {
                connect: true,
                read: true,
                write: true,
                execute: true,
                melt: true,
                mountable: false,
                allowed_melt_targets: vec!["runner".into()],
            },
        );

        let mut matrix = gitrun_core::PolicyMatrix::default();
        matrix.set(
            GitRunApi::GitDockRun,
            ApiPolicy::enabled_with([
                GitRunOperation::Connect,
                GitRunOperation::Disconnect,
                GitRunOperation::Read,
                GitRunOperation::Write,
                GitRunOperation::Execute,
                GitRunOperation::Melt,
            ]),
        );
        settings.global = matrix;
        settings.repositories.insert(
            "owner/repo".into(),
            RepositorySettings {
                docker,
                ..RepositorySettings::default()
            },
        );
        settings
    }

    #[test]
    fn denied_api_never_reaches_executor_shape() {
        let settings = GitRunSettings::default();
        let caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitDockRun,
            GitRunOperation::Connect,
            None,
            BTreeMap::from([("job".into(), "cache".into())]),
            None,
        );
        assert!(matches!(result, Err(ApiGateError::ApiDisabled { .. })));
    }

    #[test]
    fn repository_scope_can_only_reduce_global_access() {
        let settings = settings_for_dock();
        let mut caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");
        caller.repository = "other/repo".into();
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitDockRun,
            GitRunOperation::Connect,
            None,
            BTreeMap::from([("job".into(), "cache".into())]),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn dock_execute_still_requires_gsr_command_policy() {
        let settings = settings_for_dock();
        let caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitDockRun,
            GitRunOperation::Execute,
            Some("cache"),
            BTreeMap::from([("command".into(), "cargo test".into())]),
            None,
        );
        assert!(matches!(result, Err(ApiGateError::PolicyDenied { .. })));
    }

    #[test]
    fn dangerous_docked_execute_is_blocked() {
        let settings = settings_for_dock();
        let caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");
        let policy = CommandPolicy {
            baseline_blacklist: gitrun_core::PatternList::new(
                true,
                gitrun_core::baseline_patterns(),
            ),
            user_blacklist: Default::default(),
            user_whitelist: Default::default(),
        };
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitDockRun,
            GitRunOperation::Execute,
            Some("cache"),
            BTreeMap::from([("command".into(), "sudo id".into())]),
            Some(&policy),
        );
        assert!(matches!(result, Err(ApiGateError::CommandDenied { .. })));
    }

    #[test]
    fn register_permanent_uses_boolean_modifier() {
        let mut settings = GitRunSettings::default();
        let mut matrix = gitrun_core::PolicyMatrix::default();
        matrix.set(
            GitRunApi::GitRegisterRun,
            ApiPolicy::enabled_with([GitRunOperation::Register]),
        );
        settings.global = matrix;
        settings.repositories.insert(
            "owner/repo".into(),
            RepositorySettings {
                register: gitrun_core::RegisterPolicy {
                    enabled: true,
                    allow_workflow: true,
                    allow_permanent: false,
                    allowed_entries: vec!["*".into()],
                },
                ..RepositorySettings::default()
            },
        );
        let caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");

        let args = BTreeMap::from([
            ("name".into(), "tool".into()),
            ("entry".into(), "/opt/tool".into()),
            ("permanent".into(), "true".into()),
        ]);
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitRegisterRun,
            GitRunOperation::Register,
            None,
            args,
            None,
        );
        assert!(matches!(result, Err(ApiGateError::PolicyDenied { .. })));
    }

    #[test]
    fn melt_defaults_to_requesting_runner_target() {
        let settings = settings_for_dock();
        let caller = VerifiedCaller::new("owner/repo", "ci.yml", "build", "runner-1");
        let result = authorize(
            &settings,
            &caller,
            GitRunApi::GitDockRun,
            GitRunOperation::Melt,
            Some("cache"),
            BTreeMap::new(),
            None,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().arguments.get("target"), None);
    }
}
