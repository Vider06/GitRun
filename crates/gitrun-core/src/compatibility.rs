//! Static GitRun workflow compatibility analysis.
//!
//! This module never executes workflow code. It looks for capabilities that
//! the workflow appears to require and resolves them against the repository's
//! effective GitRun policy. The analyzer is intentionally conservative.

use crate::{
    api_policy::{GitRunApi, GitRunOperation},
    settings::EffectiveRepositorySettings,
    scan, Finding as WorkflowFinding,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityStatus {
    Compatible,
    Warning,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityFinding {
    pub status: CompatibilityStatus,
    pub code: String,
    pub message: String,
    pub recommendation: Option<String>,
    pub api: Option<GitRunApi>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    pub workflow: String,
    pub status: CompatibilityStatus,
    pub findings: Vec<CompatibilityFinding>,
}

pub fn analyze(
    workflow: &str,
    content: &str,
    effective: &EffectiveRepositorySettings,
) -> CompatibilityReport {
    let mut findings = Vec::new();

    add_workflow_findings(&mut findings, workflow, content);
    analyze_package_installation(&mut findings, content, effective);
    analyze_docker_access(&mut findings, content, effective);
    analyze_privileged_shells(&mut findings, content, effective);
    analyze_gitrun_calls(&mut findings, content, effective);

    let status = if findings
        .iter()
        .any(|finding| finding.status == CompatibilityStatus::Incompatible)
    {
        CompatibilityStatus::Incompatible
    } else if findings
        .iter()
        .any(|finding| finding.status == CompatibilityStatus::Warning)
    {
        CompatibilityStatus::Warning
    } else {
        CompatibilityStatus::Compatible
    };

    CompatibilityReport {
        workflow: workflow.to_owned(),
        status,
        findings,
    }
}

fn add_workflow_findings(
    findings: &mut Vec<CompatibilityFinding>,
    workflow: &str,
    content: &str,
) {
    for finding in scan(workflow, content) {
        findings.push(convert_workflow_finding(finding));
    }
}

fn convert_workflow_finding(finding: WorkflowFinding) -> CompatibilityFinding {
    CompatibilityFinding {
        status: CompatibilityStatus::Warning,
        code: format!("workflow-{}", finding.rule),
        message: finding.message,
        recommendation: Some("Review the workflow before enabling it on GitRun.".into()),
        api: None,
    }
}

fn analyze_package_installation(
    findings: &mut Vec<CompatibilityFinding>,
    content: &str,
    effective: &EffectiveRepositorySettings,
) {
    let lower = content.to_ascii_lowercase();
    let patterns = [
        "apt install",
        "apt-get install",
        "dnf install",
        "yum install",
        "pacman -s",
        "pacman -S",
    ];

    if !patterns.iter().any(|pattern| lower.contains(pattern)) {
        return;
    }

    let allowed = effective
        .api_policy
        .allows(GitRunApi::GitInstallRun, GitRunOperation::Install);

    findings.push(CompatibilityFinding {
        status: if allowed {
            CompatibilityStatus::Warning
        } else {
            CompatibilityStatus::Incompatible
        },
        code: "package-install".into(),
        message: "Workflow appears to install a system package directly.".into(),
        recommendation: Some(if allowed {
            "Prefer GitInstallRun so GitRun can choose the package manager and apply policy."
                .into()
        } else {
            "GitInstallRun is not enabled for this repository; enable it or provide the required tool another way.".into()
        }),
        api: Some(GitRunApi::GitInstallRun),
    });
}

fn analyze_docker_access(
    findings: &mut Vec<CompatibilityFinding>,
    content: &str,
    effective: &EffectiveRepositorySettings,
) {
    let lower = content.to_ascii_lowercase();
    let direct_socket = lower.contains("/var/run/docker.sock")
        || lower.contains("docker.sock")
        || lower.contains("docker_host=unix://");

    if direct_socket {
        findings.push(CompatibilityFinding {
            status: if effective.docker.direct_socket_enabled {
                CompatibilityStatus::Warning
            } else {
                CompatibilityStatus::Incompatible
            },
            code: "docker-socket".into(),
            message: "Workflow appears to require direct Docker daemon access.".into(),
            recommendation: Some(if effective.docker.direct_socket_enabled {
                "Direct Docker socket compatibility mode is enabled; keep GSR command policy enforced."
                    .into()
            } else {
                "Direct Docker socket access is disabled. Prefer GitDockRun for controlled container access, or explicitly enable the repository's Docker socket compatibility mode."
                    .into()
            }),
            api: Some(GitRunApi::GitDockRun),
        });
    }

    let docker_command = lower.contains("docker build")
        || lower.contains("docker run")
        || lower.contains("docker exec")
        || lower.contains("docker compose")
        || lower.contains("docker push");

    if docker_command && !direct_socket {
        let dock_allowed = effective
            .api_policy
            .allows(GitRunApi::GitDockRun, GitRunOperation::Execute);

        findings.push(CompatibilityFinding {
            status: if dock_allowed {
                CompatibilityStatus::Warning
            } else {
                CompatibilityStatus::Incompatible
            },
            code: "docker-command".into(),
            message: "Workflow appears to invoke the Docker CLI.".into(),
            recommendation: Some(if dock_allowed {
                "Consider moving Docker operations into an allowed GitDockRun docked container while keeping the workflow container's Docker socket disabled.".into()
            } else {
                "No allowed GitRun Docker execution capability is configured for this repository.".into()
            }),
            api: Some(GitRunApi::GitDockRun),
        });
    }
}

fn analyze_privileged_shells(
    findings: &mut Vec<CompatibilityFinding>,
    content: &str,
    effective: &EffectiveRepositorySettings,
) {
    let lower = content.to_ascii_lowercase();

    if lower.contains("sudo ") || lower.contains("sudo\n") {
        let allowed = effective
            .api_policy
            .allows(GitRunApi::GitInstallRun, GitRunOperation::Install);

        findings.push(CompatibilityFinding {
            status: CompatibilityStatus::Incompatible,
            code: "sudo".into(),
            message: "Workflow appears to require sudo privileges.".into(),
            recommendation: Some(if allowed {
                "Prefer a semantic GitRun API such as GitInstallRun instead of requesting sudo."
                    .into()
            } else {
                "GitRun does not grant sudo through the workflow API surface; configure an allowed capability or change the workflow."
                    .into()
            }),
            api: Some(GitRunApi::GitInstallRun),
        });
    }

    if lower.contains("--privileged") || lower.contains("mount /:/") {
        findings.push(CompatibilityFinding {
            status: CompatibilityStatus::Incompatible,
            code: "privileged-container".into(),
            message: "Workflow appears to request a privileged container or host-root mount."
                .into(),
            recommendation: Some(
                "Remove the privileged host access and use a specifically allowed GitRun resource instead."
                    .into(),
            ),
            api: Some(GitRunApi::GitDockRun),
        });
    }
}

fn analyze_gitrun_calls(
    findings: &mut Vec<CompatibilityFinding>,
    content: &str,
    effective: &EffectiveRepositorySettings,
) {
    for api in GitRunApi::ALL {
        let name = api.as_str();
        if !content.contains(name) {
            continue;
        }

        if !effective.api_policy.get(api).enabled {
            findings.push(CompatibilityFinding {
                status: CompatibilityStatus::Incompatible,
                code: format!("api-disabled-{name}"),
                message: format!("Workflow invokes {name}, but that API is disabled."),
                recommendation: Some(format!(
                    "Enable {name} for this repository or remove the invocation."
                )),
                api: Some(api),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiPolicy, DockerPolicy, PolicyMatrix, RepositorySettings};

    fn effective_with(
        api: GitRunApi,
        operations: &[GitRunOperation],
    ) -> EffectiveRepositorySettings {
        let mut matrix = PolicyMatrix::default();
        matrix.set(
            api,
            ApiPolicy::enabled_with(operations.iter().copied()),
        );
        let repo = RepositorySettings::default();
        let settings = crate::GitRunSettings {
            schema_version: crate::SETTINGS_SCHEMA_VERSION,
            global: matrix,
            repositories: std::collections::BTreeMap::from([("owner/repo".into(), repo)]),
        };
        settings.effective_for_repository("owner/repo")
    }

    #[test]
    fn direct_package_install_gets_gitinstallrun_recommendation() {
        let effective =
            effective_with(GitRunApi::GitInstallRun, &[GitRunOperation::Install]);
        let report = analyze(
            "ci.yml",
            "steps:\n  - run: apt-get install imagemagick\n",
            &effective,
        );
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.api == Some(GitRunApi::GitInstallRun)));
    }

    #[test]
    fn docker_socket_is_incompatible_when_disabled() {
        let effective = effective_with(
            GitRunApi::GitDockRun,
            &[GitRunOperation::Connect, GitRunOperation::Execute],
        );
        let report = analyze(
            "ci.yml",
            "env:\n  DOCKER_HOST: unix:///var/run/docker.sock\n",
            &effective,
        );
        assert_eq!(report.status, CompatibilityStatus::Incompatible);
    }

    #[test]
    fn disabled_gitrun_api_is_rejected() {
        let effective = effective_with(
            GitRunApi::GitStatusRun,
            &[GitRunOperation::Status],
        );
        let report = analyze("ci.yml", "run: GitVaultRun --read TOKEN\n", &effective);
        assert_eq!(report.status, CompatibilityStatus::Incompatible);
    }

    #[test]
    fn no_capability_pattern_is_compatible() {
        let effective = EffectiveRepositorySettings {
            api_policy: PolicyMatrix::default(),
            docker: DockerPolicy::default(),
            vault: Default::default(),
            storage: Default::default(),
            register: Default::default(),
        };
        let report = analyze("ci.yml", "run: echo hello\n", &effective);
        assert_eq!(report.status, CompatibilityStatus::Compatible);
    }
}
