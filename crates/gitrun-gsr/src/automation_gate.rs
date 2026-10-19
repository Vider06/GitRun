//! GSR authorization gate for Kitty's non-interactive automation.
//!
//! Automation mode removes per-action UI prompts, not policy checks. The
//! scheduler must supply operator-loaded settings, a configured repository,
//! and authoritative runtime identity; every grant is durably audited before
//! a permit can be returned. The permit is intentionally not constructible by
//! callers outside this module.

use crate::events::{self, SecurityEvent, Severity};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationAction {
    PredictiveWarmPool,
    StopIdleRunner,
    FastStopWorkflowRun,
}

impl AutomationAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::PredictiveWarmPool => "predictive_warm_pool",
            Self::StopIdleRunner => "stop_idle_runner",
            Self::FastStopWorkflowRun => "fast_stop_workflow_run",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutomationPolicy {
    pub kitty_enabled: bool,
    pub telemetry_enabled: bool,
    pub predictive_analytics_enabled: bool,
    pub predictive_warm_pool_enabled: bool,
    pub shadow_mode: bool,
    pub automation_mode: bool,
    pub smart_fast_stop_enabled: bool,
    pub minimum_readiness_score: f64,
    pub fast_stop_max_runtime_seconds: u64,
    pub allowed_repositories: Vec<String>,
    pub configured_repositories: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AutomationIntent {
    pub action: AutomationAction,
    pub repository: String,
    pub run_id: Option<u64>,
    pub job_id: Option<u64>,
    pub runner_id: Option<u64>,
    pub runner_name: Option<String>,
    pub readiness_score: Option<f64>,
    /// True only when Kitty's persisted decision audit identifies a previously
    /// GSR-authorized warm-pool floor still inside its hold window.
    pub retaining_previously_authorized_floor: bool,
    pub configured_min_runners: Option<u32>,
    pub proposed_min_runners: Option<u32>,
    pub max_runners: Option<u32>,
    /// Set only by the trusted scheduler after joining the exact container and
    /// GitHub registration snapshots. Unknown is not treated as idle.
    pub runner_is_verified_idle: bool,
    pub runner_is_dock_bound: bool,
    /// Set only after a fresh GitHub API lookup confirms this run is active.
    pub workflow_run_is_verified_active: bool,
    pub run_age_seconds: Option<u64>,
    pub reason: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AutomationGateError {
    #[error("automation denied: {0}")]
    Denied(String),
    #[error("GSR automation audit persistence failed: {0}")]
    Audit(String),
}

pub type Result<T> = std::result::Result<T, AutomationGateError>;

/// Opaque, short-lived grant. Only a successful GSR check can construct one.
pub struct AuthorizedAutomationAction {
    intent: AutomationIntent,
    audit_path: PathBuf,
}

impl AuthorizedAutomationAction {
    pub fn action(&self) -> AutomationAction {
        self.intent.action
    }

    pub fn intent(&self) -> &AutomationIntent {
        &self.intent
    }

    /// Record the action outcome through the same durable GSR event channel.
    /// Outcome recording failure is surfaced to the caller and must not be
    /// misreported as a successful action.
    pub fn record_outcome(&self, success: bool, outcome: &str) -> Result<()> {
        let bounded: String = outcome.chars().take(256).collect();
        let severity = if success {
            Severity::Info
        } else {
            Severity::Warning
        };
        let event = SecurityEvent::new(
            "kitty-automation-outcome",
            severity,
            format!(
                "action={} repository={} run_id={} job_id={} runner={} success={} outcome={}",
                self.intent.action.as_str(),
                self.intent.repository,
                self.intent
                    .run_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into()),
                self.intent
                    .job_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into()),
                self.intent.runner_name.as_deref().unwrap_or("-"),
                success,
                bounded
            ),
        );
        events::emit(&self.audit_path, &event)
            .map_err(|error| AutomationGateError::Audit(error.to_string()))
    }
}

pub fn authorize_action(
    policy: &AutomationPolicy,
    intent: AutomationIntent,
    audit_path: &Path,
) -> Result<AuthorizedAutomationAction> {
    let denial = validate(policy, &intent).err();

    if let Some(reason) = denial {
        let event = SecurityEvent::new(
            "kitty-automation-denied",
            Severity::Warning,
            format!(
                "action={} repository={} run_id={} job_id={} denied={}",
                intent.action.as_str(),
                intent.repository,
                intent
                    .run_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into()),
                intent
                    .job_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into()),
                reason
            ),
        );
        // Denial is never converted to a grant. Preserve the reason even if
        // the event queue itself is temporarily unavailable.
        let _ = events::emit(audit_path, &event);
        return Err(AutomationGateError::Denied(reason));
    }

    let event = SecurityEvent::new(
        "kitty-automation-authorized",
        Severity::Info,
        format!(
            "action={} repository={} run_id={} job_id={} runner={} readiness={:?} reason={}",
            intent.action.as_str(),
            intent.repository,
            intent
                .run_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            intent
                .job_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            intent.runner_name.as_deref().unwrap_or("-"),
            intent.readiness_score,
            intent.reason.chars().take(256).collect::<String>()
        ),
    );
    // Fail closed: no permit exists until GSR has durably recorded the grant.
    events::emit(audit_path, &event)
        .map_err(|error| AutomationGateError::Audit(error.to_string()))?;

    Ok(AuthorizedAutomationAction {
        intent,
        audit_path: audit_path.to_owned(),
    })
}

fn valid_repository(repository: &str) -> bool {
    let mut parts = repository.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].into_iter().all(|part| {
        !part.is_empty()
            && part.len() <= 100
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    })
}

fn validate(
    policy: &AutomationPolicy,
    intent: &AutomationIntent,
) -> std::result::Result<(), String> {
    if !valid_repository(&intent.repository) {
        return Err("invalid repository identity".into());
    }
    if !policy
        .configured_repositories
        .iter()
        .any(|repo| repo.eq_ignore_ascii_case(&intent.repository))
    {
        return Err("repository is not configured in GitRun".into());
    }
    if intent.reason.trim().is_empty() || intent.reason.len() > 1024 {
        return Err("automation intent has no bounded explanation".into());
    }

    match intent.action {
        AutomationAction::PredictiveWarmPool => {
            if !policy.kitty_enabled
                || !policy.telemetry_enabled
                || !policy.predictive_analytics_enabled
                || !policy.predictive_warm_pool_enabled
                || policy.shadow_mode
            {
                return Err("predictive warm pool is not enabled for active decisions".into());
            }
            let readiness_sufficient = intent
                .readiness_score
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                .is_some_and(|readiness| {
                    policy.minimum_readiness_score.is_finite()
                        && (0.5..=1.0).contains(&policy.minimum_readiness_score)
                        && readiness >= policy.minimum_readiness_score
                });
            if !readiness_sufficient && !intent.retaining_previously_authorized_floor {
                return Err("forecast readiness is below the operator threshold".into());
            }
            let configured = intent
                .configured_min_runners
                .ok_or_else(|| "configured runner floor is missing".to_owned())?;
            let proposed = intent
                .proposed_min_runners
                .ok_or_else(|| "proposed runner floor is missing".to_owned())?;
            let maximum = intent
                .max_runners
                .ok_or_else(|| "global runner maximum is missing".to_owned())?;
            if proposed < configured || proposed > maximum {
                return Err("proposed runner floor violates deterministic capacity bounds".into());
            }
        }
        AutomationAction::StopIdleRunner => {
            if !policy
                .allowed_repositories
                .iter()
                .any(|repo| repo.eq_ignore_ascii_case(&intent.repository))
            {
                return Err("repository is not on Kitty's explicit automation allowlist".into());
            }
            if !policy.automation_mode {
                return Err("Automation mode is disabled".into());
            }
            if intent.runner_name.as_deref().is_none_or(str::is_empty)
                || intent.runner_id.is_none_or(|runner_id| runner_id == 0)
            {
                return Err("verified runner name and numeric ID are required".into());
            }
            if !intent.runner_is_verified_idle {
                return Err("runner is not verified idle".into());
            }
            if intent.runner_is_dock_bound {
                return Err("GitDockRun owns this runner lifecycle".into());
            }
        }
        AutomationAction::FastStopWorkflowRun => {
            if !policy
                .allowed_repositories
                .iter()
                .any(|repo| repo.eq_ignore_ascii_case(&intent.repository))
            {
                return Err("repository is not on Kitty's explicit automation allowlist".into());
            }
            if !policy.automation_mode || !policy.smart_fast_stop_enabled {
                return Err("Automation mode or Smart Fast Stop is disabled".into());
            }
            if intent.run_id.is_none_or(|run_id| run_id == 0)
                || intent.job_id.is_some()
                || !intent.workflow_run_is_verified_active
            {
                return Err(
                    "Fast Stop requires an authoritative active workflow-run ID and is run-scoped, not job-scoped"
                        .into(),
                );
            }
            let age = intent
                .run_age_seconds
                .ok_or_else(|| "workflow run start time is unknown".to_owned())?;
            let maximum = policy.fast_stop_max_runtime_seconds;
            if !(300..=604_800).contains(&maximum) {
                return Err("Fast Stop runtime threshold is disabled or outside bounds".into());
            }
            if age < maximum {
                return Err("workflow run has not reached the configured stop threshold".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AutomationPolicy {
        AutomationPolicy {
            kitty_enabled: true,
            telemetry_enabled: true,
            predictive_analytics_enabled: true,
            predictive_warm_pool_enabled: true,
            shadow_mode: false,
            automation_mode: true,
            smart_fast_stop_enabled: true,
            minimum_readiness_score: 0.85,
            fast_stop_max_runtime_seconds: 3_600,
            allowed_repositories: vec!["owner/repo".into()],
            configured_repositories: vec!["owner/repo".into()],
        }
    }

    fn audit_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gitrun-gsr-automation-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("events.jsonl"))
    }

    fn intent(action: AutomationAction) -> AutomationIntent {
        AutomationIntent {
            action,
            repository: "owner/repo".into(),
            run_id: Some(42),
            job_id: None,
            runner_id: None,
            runner_name: None,
            readiness_score: Some(0.95),
            retaining_previously_authorized_floor: false,
            configured_min_runners: Some(2),
            proposed_min_runners: Some(3),
            max_runners: Some(8),
            runner_is_verified_idle: false,
            runner_is_dock_bound: false,
            workflow_run_is_verified_active: true,
            run_age_seconds: Some(7_200),
            reason: "test policy".into(),
        }
    }

    #[test]
    fn active_warm_pool_requires_explicit_bounds_and_readiness() {
        let (dir, path) = audit_path();
        let permit = authorize_action(
            &policy(),
            intent(AutomationAction::PredictiveWarmPool),
            &path,
        )
        .unwrap();
        assert_eq!(permit.action(), AutomationAction::PredictiveWarmPool);
        drop(permit);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fast_stop_requires_a_whitelisted_verified_run_and_expired_threshold() {
        let (dir, path) = audit_path();
        let permit = authorize_action(
            &policy(),
            intent(AutomationAction::FastStopWorkflowRun),
            &path,
        )
        .unwrap();
        assert_eq!(permit.intent().run_id, Some(42));
        let mut denied = intent(AutomationAction::FastStopWorkflowRun);
        denied.job_id = Some(7);
        assert!(authorize_action(&policy(), denied, &path).is_err());
        drop(permit);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn automation_cannot_stop_a_busy_or_dock_bound_runner() {
        let (dir, path) = audit_path();
        let mut request = intent(AutomationAction::StopIdleRunner);
        request.runner_name = Some("runner-1".into());
        assert!(authorize_action(&policy(), request.clone(), &path).is_err());
        request.runner_is_verified_idle = true;
        request.runner_is_dock_bound = true;
        assert!(authorize_action(&policy(), request, &path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
