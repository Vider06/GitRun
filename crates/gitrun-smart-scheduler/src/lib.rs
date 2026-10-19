//! Optional, deterministic-statistics-based predictive scheduling for GitRun.
//!
//! Kitty is an add-on, not the scheduler. The deterministic reconciler remains
//! authoritative and is used unchanged when Kitty is disabled, stale, invalid,
//! unavailable, or in shadow mode. This crate emits only a bounded runner-floor
//! recommendation; it has no GitHub, Docker, shell, secret-store, or GSR authority.

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const SETTINGS_RELATIVE_PATH: &str = "kitty/settings.json";
pub const BUCKET_SECONDS: u64 = 300;
pub const MODEL_VERSION: u32 = 1;
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_MODEL_BYTES: u64 = 1024 * 1024;
const MAX_HISTORY_BUCKETS: usize = 2016; // Seven days, sampled every five minutes.
const MAX_MODELS: usize = 64;
const MAX_AUDIT_RECORDS: usize = 128;
const MAX_EVALUATED_FORECASTS: usize = 2016;
const FORECAST_HORIZON_SECONDS: u64 = 3600;
const FORECAST_HORIZON_BUCKETS: u64 = FORECAST_HORIZON_SECONDS / BUCKET_SECONDS;
const SETTINGS_RELOAD_INTERVAL: Duration = Duration::from_secs(30);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum KittyError {
    #[error("Kitty storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Kitty JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid Kitty data: {0}")]
    Invalid(String),
    #[error("unsupported Kitty settings schema version {0}")]
    UnsupportedSchema(u32),
    #[error("Kitty repository model limit ({MAX_MODELS}) reached")]
    ModelLimit,
}

pub type Result<T> = std::result::Result<T, KittyError>;

/// Operator-controlled capabilities. Missing settings means all Kitty activity
/// is off. Enabling the master switch only enables data collection by default;
/// automated capacity growth also requires its own switch and shadow mode off.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct KittySettings {
    pub schema_version: u32,
    pub enabled: bool,
    pub telemetry_enabled: bool,
    pub predictive_analytics_enabled: bool,
    pub predictive_warm_pool_enabled: bool,
    /// Reserved until reliable per-job resource measurements and bounded
    /// runner profiles are available. It never authorizes a resource change.
    pub adaptive_resource_management_enabled: bool,
    /// Reserved until reliable active-job duration and cancellation outcomes
    /// are modeled. It never submits a cancellation request in this version.
    pub smart_fast_stop_enabled: bool,
    pub shadow_mode: bool,
    /// Five-minute buckets required before a forecast can be considered.
    /// The default requires two days of observed history.
    pub minimum_samples: usize,
    /// Bounded rolling history, at most seven days.
    pub history_buckets: usize,
    /// Upper bound on how far Kitty may raise the deterministic minimum.
    pub max_additional_warm_runners: u32,
    /// Readiness threshold; a quality/readiness score, not a probability.
    pub minimum_quality_score: f64,
}

impl Default for KittySettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            enabled: false,
            telemetry_enabled: true,
            predictive_analytics_enabled: true,
            predictive_warm_pool_enabled: false,
            adaptive_resource_management_enabled: false,
            smart_fast_stop_enabled: false,
            shadow_mode: true,
            minimum_samples: 576,
            history_buckets: MAX_HISTORY_BUCKETS,
            max_additional_warm_runners: 2,
            minimum_quality_score: 0.85,
        }
    }
}

impl KittySettings {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(KittyError::UnsupportedSchema(self.schema_version));
        }
        if !(24..=MAX_HISTORY_BUCKETS).contains(&self.history_buckets) {
            return Err(KittyError::Invalid(format!(
                "history_buckets must be between 24 and {MAX_HISTORY_BUCKETS}"
            )));
        }
        if !(24..=self.history_buckets).contains(&self.minimum_samples) {
            return Err(KittyError::Invalid(
                "minimum_samples must be between 24 and history_buckets".into(),
            ));
        }
        if self.max_additional_warm_runners > 100 {
            return Err(KittyError::Invalid(
                "max_additional_warm_runners must not exceed 100".into(),
            ));
        }
        if !self.minimum_quality_score.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_quality_score)
        {
            return Err(KittyError::Invalid(
                "minimum_quality_score must be a finite number between 0 and 1".into(),
            ));
        }
        Ok(())
    }
}

/// Aggregated input for one repository. It deliberately excludes job payloads,
/// workflow YAML, environment values, credentials, tokens, and individual job
/// names. Resource values are optional because they are not always attributable
/// to this repository or to a specific workload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub bucket_epoch: u64,
    pub queued_jobs: u32,
    pub busy_runners: u32,
    pub online_runners: u32,
    pub managed_runners: u32,
    pub host_cpu_percent: Option<f64>,
    pub host_memory_percent: Option<f64>,
    pub host_disk_percent: Option<f64>,
    #[serde(default = "one")]
    pub snapshots_in_bucket: u32,
}

fn one() -> u32 {
    1
}

impl Observation {
    pub fn demand(&self) -> u32 {
        self.queued_jobs.saturating_add(self.busy_runners)
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("host_cpu_percent", self.host_cpu_percent),
            ("host_memory_percent", self.host_memory_percent),
            ("host_disk_percent", self.host_disk_percent),
        ] {
            if value.is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v)) {
                return Err(KittyError::Invalid(format!(
                    "{name} must be a finite percentage from 0 to 100"
                )));
            }
        }
        if self.bucket_epoch % BUCKET_SECONDS != 0 {
            return Err(KittyError::Invalid(
                "observation timestamp is not aligned to a five-minute bucket".into(),
            ));
        }
        Ok(())
    }

    fn merge_snapshot(&mut self, next: &Self) {
        // Preserve short queue/busy bursts instead of averaging them away.
        self.queued_jobs = self.queued_jobs.max(next.queued_jobs);
        self.busy_runners = self.busy_runners.max(next.busy_runners);
        self.online_runners = next.online_runners;
        self.managed_runners = next.managed_runners;
        self.host_cpu_percent = next.host_cpu_percent;
        self.host_memory_percent = next.host_memory_percent;
        self.host_disk_percent = next.host_disk_percent;
        self.snapshots_in_bucket = self.snapshots_in_bucket.saturating_add(1);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Forecast {
    pub model_version: u32,
    pub generated_at_epoch: u64,
    pub target_epoch: u64,
    pub horizon_seconds: u64,
    pub sample_count: usize,
    pub evaluated_forecasts: usize,
    pub predicted_peak_concurrency: u32,
    pub mean_absolute_error: Option<f64>,
    pub quality_score: f64,
    /// Readiness score combines data volume, forecast quality and freshness.
    /// It is explicitly not a calibrated probability.
    pub confidence_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PredictionRecord {
    pub issued_at_bucket: u64,
    pub target_bucket: u64,
    pub predicted_demand: u32,
    pub actual_demand: Option<u32>,
    pub skipped_missing_observation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionAudit {
    pub bucket_epoch: u64,
    pub model_version: u32,
    pub source_window_start: Option<u64>,
    pub source_window_end: Option<u64>,
    pub status: DecisionStatus,
    pub reason: String,
    pub configured_min_runners: u32,
    pub proposed_min_runners: u32,
    pub effective_min_runners: u32,
    pub predicted_peak_concurrency: Option<u32>,
    pub confidence_score: Option<f64>,
    pub sample_count: usize,
    pub planned_action_count: Option<usize>,
    pub execution_outcome: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Disabled,
    TelemetryDisabled,
    Observing,
    InsufficientHistory,
    ShadowRecommendation,
    RecommendationOnly,
    AppliedWarmPoolFloor,
    ResourcePressureHold,
    SafeFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowModel {
    pub schema_version: u32,
    pub model_version: u32,
    pub repository: String,
    pub generated_at_epoch: u64,
    pub history: Vec<Observation>,
    pub predictions: Vec<PredictionRecord>,
    pub latest_forecast: Option<Forecast>,
    pub decision_audit: Vec<DecisionAudit>,
}

impl WorkflowModel {
    fn empty(repository: &str) -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            model_version: MODEL_VERSION,
            repository: repository.to_owned(),
            generated_at_epoch: 0,
            history: Vec::new(),
            predictions: Vec::new(),
            latest_forecast: None,
            decision_audit: Vec::new(),
        }
    }

    fn validate_for(&self, repository: &str, history_limit: usize) -> Result<()> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(KittyError::UnsupportedSchema(self.schema_version));
        }
        if self.model_version != MODEL_VERSION {
            return Err(KittyError::Invalid(format!(
                "unsupported model algorithm version {}",
                self.model_version
            )));
        }
        if self.repository != repository {
            return Err(KittyError::Invalid(
                "stored repository identity does not match requested repository".into(),
            ));
        }
        if self.history.len() > history_limit
            || self.predictions.len() > MAX_EVALUATED_FORECASTS
            || self.decision_audit.len() > MAX_AUDIT_RECORDS
        {
            return Err(KittyError::Invalid(
                "stored model exceeds its bounded history limits".into(),
            ));
        }
        let mut previous = None;
        for item in &self.history {
            item.validate()?;
            if previous.is_some_and(|value| value >= item.bucket_epoch) {
                return Err(KittyError::Invalid(
                    "stored observations are not strictly chronological".into(),
                ));
            }
            previous = Some(item.bucket_epoch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KittyDecision {
    pub repository: String,
    pub status: DecisionStatus,
    pub reason: String,
    pub configured_min_runners: u32,
    pub proposed_min_runners: u32,
    pub effective_min_runners: u32,
    pub predicted_peak_concurrency: Option<u32>,
    pub confidence_score: Option<f64>,
    pub sample_count: usize,
    pub shadow_mode: bool,
    pub decision_bucket: Option<u64>,
}

impl KittyDecision {
    pub fn fallback(repository: &str, configured_min_runners: u32, reason: impl Into<String>) -> Self {
        Self {
            repository: repository.to_owned(),
            status: DecisionStatus::SafeFallback,
            reason: reason.into(),
            configured_min_runners,
            proposed_min_runners: configured_min_runners,
            effective_min_runners: configured_min_runners,
            predicted_peak_concurrency: None,
            confidence_score: None,
            sample_count: 0,
            shadow_mode: true,
            decision_bucket: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObservationInput {
    pub repository: String,
    pub observed_at_epoch: u64,
    pub queued_jobs: u32,
    pub busy_runners: u32,
    pub online_runners: u32,
    pub managed_runners: u32,
    pub host_cpu_percent: Option<f64>,
    pub host_memory_percent: Option<f64>,
    pub host_disk_percent: Option<f64>,
}

pub struct KittyRuntime {
    state_dir: PathBuf,
    settings: KittySettings,
    settings_error: Option<String>,
    last_settings_reload: Option<Instant>,
    models: HashMap<String, WorkflowModel>,
    disabled_models: HashSet<String>,
    dirty_models: HashSet<String>,
}

impl KittyRuntime {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            settings: KittySettings::default(),
            settings_error: None,
            last_settings_reload: None,
            models: HashMap::new(),
            disabled_models: HashSet::new(),
            dirty_models: HashSet::new(),
        }
    }

    pub fn settings_path(state_dir: &Path) -> PathBuf {
        state_dir.join(SETTINGS_RELATIVE_PATH)
    }

    pub fn settings(&self) -> &KittySettings {
        &self.settings
    }

    pub fn observe(
        &mut self,
        input: ObservationInput,
        configured_min_runners: u32,
        max_runners: u32,
        capacity_growth_allowed: bool,
    ) -> Result<KittyDecision> {
        self.reload_settings_if_due();

        if let Some(error) = &self.settings_error {
            return Ok(KittyDecision::fallback(
                &input.repository,
                configured_min_runners,
                format!("Kitty settings invalid; deterministic scheduler unchanged: {error}"),
            ));
        }
        if !self.settings.enabled {
            return Ok(self.simple_decision(
                &input.repository,
                DecisionStatus::Disabled,
                "Kitty is disabled; deterministic scheduler remains authoritative",
                configured_min_runners,
            ));
        }
        if !self.settings.telemetry_enabled {
            return Ok(self.simple_decision(
                &input.repository,
                DecisionStatus::TelemetryDisabled,
                "Kitty telemetry is disabled; no model data is collected",
                configured_min_runners,
            ));
        }
        if !valid_repository(&input.repository) {
            return Ok(KittyDecision::fallback(
                &input.repository,
                configured_min_runners,
                "repository identity is malformed; Kitty refused to store observations",
            ));
        }
        if input.observed_at_epoch == 0 {
            return Ok(KittyDecision::fallback(
                &input.repository,
                configured_min_runners,
                "observation timestamp is invalid",
            ));
        }

        let bucket = input.observed_at_epoch / BUCKET_SECONDS * BUCKET_SECONDS;
        let observation = Observation {
            bucket_epoch: bucket,
            queued_jobs: input.queued_jobs,
            busy_runners: input.busy_runners,
            online_runners: input.online_runners,
            managed_runners: input.managed_runners,
            host_cpu_percent: input.host_cpu_percent,
            host_memory_percent: input.host_memory_percent,
            host_disk_percent: input.host_disk_percent,
            snapshots_in_bucket: 1,
        };
        observation.validate()?;

        let key = repository_key(&input.repository);
        if self.disabled_models.contains(&key) {
            return Ok(KittyDecision::fallback(
                &input.repository,
                configured_min_runners,
                "repository model failed validation earlier; delete or repair its model file to re-enable Kitty",
            ));
        }
        if !self.models.contains_key(&key) {
            if self.models.len() >= MAX_MODELS {
                return Ok(KittyDecision::fallback(
                    &input.repository,
                    configured_min_runners,
                    "Kitty model capacity reached; deterministic scheduler unchanged",
                ));
            }
            match self.load_model(&input.repository, &key) {
                Ok(model) => {
                    self.models.insert(key.clone(), model);
                }
                Err(error) => {
                    self.disabled_models.insert(key.clone());
                    eprintln!(
                        "gitrun-smart-scheduler: model for {} failed closed: {}",
                        input.repository, error
                    );
                    return Ok(KittyDecision::fallback(
                        &input.repository,
                        configured_min_runners,
                        format!("stored model is invalid or unsafe: {error}"),
                    ));
                }
            }
        }

        let model = self.models.get_mut(&key).ok_or_else(|| {
            KittyError::Invalid("model disappeared during a single-threaded update".into())
        })?;
        model.validate_for(&input.repository, self.settings.history_buckets)?;

        let is_new_bucket = match model.history.last() {
            None => true,
            Some(last) if bucket > last.bucket_epoch => true,
            Some(last) if bucket == last.bucket_epoch => false,
            Some(_) => {
                return Ok(KittyDecision::fallback(
                    &input.repository,
                    configured_min_runners,
                    "system clock moved backwards; predictive action withheld",
                ));
            }
        };

        if let Some(last) = model.history.last_mut().filter(|last| last.bucket_epoch == bucket) {
            last.merge_snapshot(&observation);
        } else {
            model.history.push(observation);
            if model.history.len() > self.settings.history_buckets {
                let excess = model.history.len() - self.settings.history_buckets;
                model.history.drain(0..excess);
            }
            evaluate_matured_predictions(model, bucket);
            model.generated_at_epoch = input.observed_at_epoch;
            if self.settings.predictive_analytics_enabled {
                model.latest_forecast =
                    compute_forecast(model, &self.settings, input.observed_at_epoch);
                if let Some(forecast) = &model.latest_forecast {
                    model.predictions.push(PredictionRecord {
                        issued_at_bucket: bucket,
                        target_bucket: bucket.saturating_add(FORECAST_HORIZON_SECONDS),
                        predicted_demand: forecast.predicted_peak_concurrency,
                        actual_demand: None,
                        skipped_missing_observation: false,
                    });
                }
            } else {
                model.latest_forecast = None;
            }
            if model.predictions.len() > MAX_EVALUATED_FORECASTS {
                let excess = model.predictions.len() - MAX_EVALUATED_FORECASTS;
                model.predictions.drain(0..excess);
            }
            self.dirty_models.insert(key.clone());
        }

        let forecast = if self.settings.predictive_analytics_enabled {
            model
                .latest_forecast
                .as_ref()
                .filter(|forecast| {
                    input.observed_at_epoch.saturating_sub(forecast.generated_at_epoch)
                        <= BUCKET_SECONDS.saturating_mul(2)
                })
        } else {
            None
        };
        let sample_count = model.history.len();
        let proposed_min = forecast
            .filter(|f| {
                self.settings.predictive_warm_pool_enabled
                    && f.confidence_score >= self.settings.minimum_quality_score
            })
            .map(|f| {
                let desired = f.predicted_peak_concurrency;
                let additional = desired
                    .saturating_sub(configured_min_runners)
                    .min(self.settings.max_additional_warm_runners);
                configured_min_runners
                    .saturating_add(additional)
                    .min(max_runners)
                    .max(configured_min_runners.min(max_runners))
            })
            .unwrap_or(configured_min_runners.min(max_runners));

        let capability_note = if self.settings.adaptive_resource_management_enabled
            || self.settings.smart_fast_stop_enabled
        {
            " Adaptive resource automation and Smart Fast Stop triggers are reserved capabilities in this version and are not executed."
        } else {
            ""
        };

        let (status, effective_min, reason) = if !self.settings.predictive_analytics_enabled {
            (
                DecisionStatus::Observing,
                configured_min_runners,
                "Telemetry-only mode; predictive analytics is disabled.".to_owned(),
            )
        } else if forecast.is_none() {
            (
                DecisionStatus::InsufficientHistory,
                configured_min_runners,
                format!(
                    "Collecting bounded five-minute observations; {} samples are available and {} are required. No predictive action is permitted.",
                    sample_count, self.settings.minimum_samples
                ),
            )
        } else {
            let f = forecast.expect("checked forecast");
            let acceptable = f.confidence_score >= self.settings.minimum_quality_score
                && sample_count >= self.settings.minimum_samples;
            if !acceptable {
                (
                    DecisionStatus::InsufficientHistory,
                    configured_min_runners,
                    format!(
                        "Forecast readiness {:.3} is below the configured threshold {:.3}; no action.",
                        f.confidence_score, self.settings.minimum_quality_score
                    ),
                )
            } else if !self.settings.predictive_warm_pool_enabled
                || proposed_min <= configured_min_runners
            {
                (
                    DecisionStatus::RecommendationOnly,
                    configured_min_runners,
                    format!(
                        "Forecast demand is {}; capacity automation is disabled or the forecast does not exceed the deterministic floor.",
                        f.predicted_peak_concurrency
                    ),
                )
            } else if self.settings.shadow_mode {
                (
                    DecisionStatus::ShadowRecommendation,
                    configured_min_runners,
                    format!(
                        "Shadow recommendation: raise the runner floor from {} to {} for the one-hour demand forecast. No capacity change applied.",
                        configured_min_runners, proposed_min
                    ),
                )
            } else if !capacity_growth_allowed {
                (
                    DecisionStatus::ResourcePressureHold,
                    configured_min_runners,
                    "Forecast prewarming withheld by existing scheduler resource-pressure backpressure.".to_owned(),
                )
            } else {
                (
                    DecisionStatus::AppliedWarmPoolFloor,
                    proposed_min,
                    format!(
                        "Predictive warm-pool floor raised within explicit bounds: {} to {} (forecast {}, readiness {:.3}).",
                        configured_min_runners,
                        proposed_min,
                        f.predicted_peak_concurrency,
                        f.confidence_score
                    ),
                )
            }
        };

        let mut decision = KittyDecision {
            repository: input.repository.clone(),
            status,
            reason: format!("{reason}{capability_note}"),
            configured_min_runners,
            proposed_min_runners: proposed_min,
            effective_min_runners: effective_min,
            predicted_peak_concurrency: forecast.map(|f| f.predicted_peak_concurrency),
            confidence_score: forecast.map(|f| f.confidence_score),
            sample_count,
            shadow_mode: self.settings.shadow_mode,
            decision_bucket: Some(bucket),
        };
        if decision.effective_min_runners > max_runners {
            // Defensive final bound at the integration boundary.
            decision.effective_min_runners = configured_min_runners.min(max_runners);
            decision.status = DecisionStatus::SafeFallback;
            decision.reason =
                "Kitty proposal exceeded the hard runner maximum; deterministic fallback applied."
                    .into();
        }

        let audit = DecisionAudit {
            bucket_epoch: bucket,
            model_version: model.model_version,
            source_window_start: model.history.first().map(|sample| sample.bucket_epoch),
            source_window_end: model.history.last().map(|sample| sample.bucket_epoch),
            status: decision.status,
            reason: decision.reason.clone(),
            configured_min_runners,
            proposed_min_runners: decision.proposed_min_runners,
            effective_min_runners: decision.effective_min_runners,
            predicted_peak_concurrency: decision.predicted_peak_concurrency,
            confidence_score: decision.confidence_score,
            sample_count,
            planned_action_count: None,
            execution_outcome: None,
        };
        let should_add_audit = model
            .decision_audit
            .last()
            .map(|last| {
                last.bucket_epoch != audit.bucket_epoch
                    || last.status != audit.status
                    || last.effective_min_runners != audit.effective_min_runners
                    || last.reason != audit.reason
            })
            .unwrap_or(true);
        if should_add_audit {
            if matches!(
                decision.status,
                DecisionStatus::ShadowRecommendation
                    | DecisionStatus::AppliedWarmPoolFloor
                    | DecisionStatus::ResourcePressureHold
                    | DecisionStatus::SafeFallback
            ) {
                eprintln!(
                    "gitrun-smart-scheduler: repository={} status={:?} configured_min={} proposed_min={} effective_min={} predicted={:?} quality={:?}: {}",
                    decision.repository,
                    decision.status,
                    decision.configured_min_runners,
                    decision.proposed_min_runners,
                    decision.effective_min_runners,
                    decision.predicted_peak_concurrency,
                    decision.confidence_score,
                    decision.reason
                );
            }
            model.decision_audit.push(audit);
            if model.decision_audit.len() > MAX_AUDIT_RECORDS {
                let excess = model.decision_audit.len() - MAX_AUDIT_RECORDS;
                model.decision_audit.drain(0..excess);
            }
            self.dirty_models.insert(key);
        }

        // A live capacity change is impossible unless its model and decision
        // audit are durable before the deterministic planner sees the raised floor.
        if decision.status == DecisionStatus::AppliedWarmPoolFloor
            && self.dirty_models.contains(&key)
        {
            if let Err(error) = self.persist_model(&key) {
                self.disabled_models.insert(key.clone());
                return Err(error);
            }
            self.dirty_models.remove(&key);
        }

        Ok(decision)
    }

    /// Records whether the existing deterministic plan ran, had no actions, or
    /// stopped on an error. It never changes the plan or grants authority.
    pub fn finish_cycle(
        &mut self,
        decision: &KittyDecision,
        planned_action_count: usize,
        action_execution_succeeded: bool,
    ) -> Result<()> {
        let Some(bucket) = decision.decision_bucket else {
            return Ok(());
        };
        let key = repository_key(&decision.repository);
        let Some(model) = self.models.get_mut(&key) else {
            return Ok(());
        };
        if let Some(audit) = model
            .decision_audit
            .iter_mut()
            .rev()
            .find(|audit| audit.bucket_epoch == bucket)
        {
            if audit.execution_outcome.is_none() {
                audit.planned_action_count = Some(planned_action_count);
                audit.execution_outcome = Some(if !action_execution_succeeded {
                    "action_failure".into()
                } else if planned_action_count == 0 {
                    "no_actions_required".into()
                } else {
                    "plan_executed".into()
                });
                self.dirty_models.insert(key.clone());
            }
        }
        if self.dirty_models.remove(&key) {
            if let Err(error) = self.persist_model(&key) {
                self.disabled_models.insert(key.clone());
                return Err(error);
            }
        }
        Ok(())
    }

    fn simple_decision(
        &self,
        repository: &str,
        status: DecisionStatus,
        reason: &str,
        configured_min_runners: u32,
    ) -> KittyDecision {
        KittyDecision {
            repository: repository.to_owned(),
            status,
            reason: reason.to_owned(),
            configured_min_runners,
            proposed_min_runners: configured_min_runners,
            effective_min_runners: configured_min_runners,
            predicted_peak_concurrency: None,
            confidence_score: None,
            sample_count: 0,
            shadow_mode: self.settings.shadow_mode,
            decision_bucket: None,
        }
    }

    fn reload_settings_if_due(&mut self) {
        let due = self
            .last_settings_reload
            .map(|last| last.elapsed() >= SETTINGS_RELOAD_INTERVAL)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_settings_reload = Some(Instant::now());
        match read_settings(&Self::settings_path(&self.state_dir)) {
            Ok(settings) => {
                self.settings = settings;
                self.settings_error = None;
            }
            Err(error) => {
                self.settings = KittySettings::default();
                self.settings_error = Some(error.to_string());
                eprintln!(
                    "gitrun-smart-scheduler: Kitty disabled because settings could not be validated: {error}"
                );
            }
        }
    }

    fn load_model(&self, repository: &str, key: &str) -> Result<WorkflowModel> {
        let dir = self.state_dir.join("kitty");
        if !validate_read_data_dir(&dir)? {
            return Ok(WorkflowModel::empty(repository));
        }
        let path = dir.join(format!("model-{key}.json"));
        match read_checked(&path, MAX_MODEL_BYTES)? {
            None => Ok(WorkflowModel::empty(repository)),
            Some(bytes) => {
                let mut model: WorkflowModel = serde_json::from_slice(&bytes)?;
                model.validate_for(repository, self.settings.history_buckets)?;
                model.history.shrink_to_fit();
                model.predictions.shrink_to_fit();
                Ok(model)
            }
        }
    }

    fn persist_model(&self, key: &str) -> Result<()> {
        let model = self.models.get(key).ok_or_else(|| {
            KittyError::Invalid("attempted to persist an absent model".into())
        })?;
        ensure_private_data_dir(&self.state_dir.join("kitty"))?;
        let path = self
            .state_dir
            .join("kitty")
            .join(format!("model-{key}.json"));
        let json = serde_json::to_vec(model)?;
        if json.len() as u64 > MAX_MODEL_BYTES {
            return Err(KittyError::Invalid(
                "serialized model exceeded the one-megabyte bound".into(),
            ));
        }
        atomic_write_private(&path, &json)?;
        Ok(())
    }
}

fn read_settings(path: &Path) -> Result<KittySettings> {
    let parent = path
        .parent()
        .ok_or_else(|| KittyError::Invalid("settings path has no parent".into()))?;
    if !validate_read_data_dir(parent)? {
        return Ok(KittySettings::default());
    }
    if let Some(bytes) = read_checked(path, MAX_SETTINGS_BYTES)? {
        let settings: KittySettings = serde_json::from_slice(&bytes)?;
        settings.validate()?;
        Ok(settings)
    } else {
        Ok(KittySettings::default())
    }
}

fn compute_forecast(
    model: &WorkflowModel,
    settings: &KittySettings,
    now_epoch: u64,
) -> Option<Forecast> {
    let history = &model.history;
    if history.len() < settings.minimum_samples {
        return None;
    }
    let latest = history.last()?;
    let age = now_epoch.saturating_sub(latest.bucket_epoch);
    if age > BUCKET_SECONDS.saturating_mul(2) {
        return None;
    }
    let start = history.get(history.len().saturating_sub(settings.minimum_samples))?;
    let required_span = ((settings.minimum_samples as u64).saturating_sub(1))
        .saturating_mul(BUCKET_SECONDS)
        .saturating_mul(9)
        / 10;
    if latest.bucket_epoch.saturating_sub(start.bucket_epoch) < required_span {
        return None;
    }

    let demands: Vec<f64> = history.iter().map(|observation| observation.demand() as f64).collect();
    let recent_start = demands.len().saturating_sub(12);
    let previous_start = recent_start.saturating_sub(12);
    let recent = &demands[recent_start..];
    let previous = &demands[previous_start..recent_start];
    if recent.is_empty() || previous.is_empty() {
        return None;
    }
    let recent_mean = mean(recent);
    let previous_mean = mean(previous);
    let trend = recent_mean - previous_mean;
    let mut ewma = recent[0];
    for value in recent.iter().skip(1) {
        ewma = 0.35 * value + 0.65 * ewma;
    }
    let predicted = (ewma + trend).max(recent_mean).max(0.0).ceil();
    let predicted_peak_concurrency = predicted.min(u32::MAX as f64) as u32;

    let evaluated: Vec<&PredictionRecord> = model
        .predictions
        .iter()
        .filter(|record| record.actual_demand.is_some() && !record.skipped_missing_observation)
        .rev()
        .take(288)
        .collect();
    let (mae, quality_score) = if evaluated.len() >= 24 {
        let errors: Vec<f64> = evaluated
            .iter()
            .filter_map(|record| {
                record
                    .actual_demand
                    .map(|actual| (record.predicted_demand as f64 - actual as f64).abs())
            })
            .collect();
        let error = mean(&errors);
        let actual_mean = mean(
            &evaluated
                .iter()
                .filter_map(|record| record.actual_demand.map(|v| v as f64))
                .collect::<Vec<_>>(),
        );
        let quality = (1.0 - error / actual_mean.max(1.0)).clamp(0.0, 1.0);
        (Some(error), quality)
    } else {
        (None, 0.0)
    };
    let sample_factor = (history.len() as f64 / settings.minimum_samples as f64).clamp(0.0, 1.0);
    let evaluation_factor = (evaluated.len() as f64 / 48.0).clamp(0.0, 1.0);
    let freshness_factor = (1.0 - age as f64 / (BUCKET_SECONDS.saturating_mul(3) as f64))
        .clamp(0.0, 1.0);
    let confidence_score = (sample_factor * evaluation_factor * quality_score * freshness_factor)
        .clamp(0.0, 1.0);

    Some(Forecast {
        model_version: MODEL_VERSION,
        generated_at_epoch: now_epoch,
        target_epoch: now_epoch.saturating_add(FORECAST_HORIZON_SECONDS),
        horizon_seconds: FORECAST_HORIZON_SECONDS,
        sample_count: history.len(),
        evaluated_forecasts: evaluated.len(),
        predicted_peak_concurrency,
        mean_absolute_error: mae,
        quality_score,
        confidence_score,
    })
}

fn evaluate_matured_predictions(model: &mut WorkflowModel, current_bucket: u64) {
    let actuals: HashMap<u64, u32> = model
        .history
        .iter()
        .map(|sample| (sample.bucket_epoch, sample.demand()))
        .collect();
    for prediction in &mut model.predictions {
        if prediction.actual_demand.is_some() || prediction.skipped_missing_observation {
            continue;
        }
        if let Some(actual) = actuals.get(&prediction.target_bucket) {
            prediction.actual_demand = Some(*actual);
        } else if current_bucket
            > prediction.target_bucket.saturating_add(BUCKET_SECONDS)
        {
            prediction.skipped_missing_observation = true;
        }
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
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

fn repository_key(repository: &str) -> String {
    // Stable, non-cryptographic filename hash. The repository name is verified
    // inside the model before use; this hash is only a bounded path component.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in repository.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn validate_read_data_dir(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KittyError::Invalid(format!(
            "{} must be a real directory, not a symlink",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(KittyError::Invalid(format!(
                "{} must be service-owned and private to the owner (mode 0700)",
                path.display()
            )));
        }
    }
    Ok(true)
}

fn read_checked(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Err(KittyError::Invalid(format!(
            "{} must be a regular, bounded, non-symlink file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(KittyError::Invalid(format!(
                "{} has an unsafe owner or hard-link count",
                path.display()
            )));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > max_bytes {
        return Err(KittyError::Invalid(format!(
            "{} changed during validation",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 || opened.uid() != unsafe { libc::geteuid() } {
            return Err(KittyError::Invalid(format!(
                "{} has an unsafe opened-file owner or hard-link count",
                path.display()
            )));
        }
    }
    let mut bytes = Vec::with_capacity(opened.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(KittyError::Invalid(format!(
            "{} exceeded its read-size bound",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

fn ensure_private_data_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KittyError::Invalid(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(KittyError::Invalid(format!(
                "{} is not owned by the Kitty service identity",
                path.display()
            )));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KittyError::Invalid("model path has no parent directory".into()))?;
    ensure_private_data_dir(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(KittyError::Invalid(format!(
                "{} was replaced with an unsafe file type",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(KittyError::Invalid(format!(
                    "{} has an unsafe owner or hard-link count",
                    path.display()
                )));
            }
        }
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".kitty-tmp-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gitrun-kitty-{label}-{}-{now}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn enabled_settings() -> KittySettings {
        KittySettings {
            enabled: true,
            minimum_samples: 24,
            history_buckets: 48,
            ..KittySettings::default()
        }
    }

    fn input(bucket: u64, queue: u32, busy: u32) -> ObservationInput {
        ObservationInput {
            repository: "owner/repo".into(),
            observed_at_epoch: bucket,
            queued_jobs: queue,
            busy_runners: busy,
            online_runners: 2,
            managed_runners: 3,
            host_cpu_percent: Some(12.0),
            host_memory_percent: Some(30.0),
            host_disk_percent: Some(20.0),
        }
    }

    fn write_settings(state: &Path, settings: &KittySettings) {
        let path = KittyRuntime::settings_path(state);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(settings).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                path.parent().unwrap(),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::set_permissions(
                KittyRuntime::settings_path(state),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }

    #[test]
    fn kitty_is_disabled_by_default_and_never_changes_deterministic_floor() {
        let dir = temp_dir("default-off");
        let mut runtime = KittyRuntime::new(&dir);
        let decision = runtime.observe(input(1_800, 0, 1), 3, 8, true).unwrap();
        assert_eq!(decision.status, DecisionStatus::Disabled);
        assert_eq!(decision.effective_min_runners, 3);
        assert!(!dir.join("kitty").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_reject_out_of_range_or_non_finite_thresholds() {
        let mut settings = enabled_settings();
        settings.minimum_quality_score = f64::NAN;
        assert!(settings.validate().is_err());
        settings.minimum_quality_score = 0.85;
        settings.history_buckets = MAX_HISTORY_BUCKETS + 1;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn malformed_settings_fail_closed_without_stopping_scheduler() {
        let dir = temp_dir("bad-settings");
        let path = KittyRuntime::settings_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"schema_version":1,"enabled":true,"surprise":true}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut runtime = KittyRuntime::new(&dir);
        let decision = runtime.observe(input(1_800, 0, 1), 3, 8, true).unwrap();
        assert_eq!(decision.status, DecisionStatus::SafeFallback);
        assert_eq!(decision.effective_min_runners, 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn warm_pool_is_shadow_only_until_explicitly_enabled_and_is_bounded() {
        let dir = temp_dir("shadow");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = true;
        settings.minimum_quality_score = 0.0;
        settings.minimum_samples = 24;
        settings.max_additional_warm_runners = 1;
        write_settings(&dir, &settings);
        let mut runtime = KittyRuntime::new(&dir);
        for step in 0..60 {
            let stamp = 10_000 + step * BUCKET_SECONDS;
            // Build a stable, non-zero series and score enough one-hour forecasts.
            let _ = runtime.observe(input(stamp, 1, 3), 2, 4, true).unwrap();
            let decision = runtime.observe(input(stamp + 1, 1, 3), 2, 4, true).unwrap();
            runtime.finish_cycle(&decision, 0, true).unwrap();
        }
        let now = 10_000 + 59 * BUCKET_SECONDS;
        let decision = runtime.observe(input(now + 2, 3, 3), 2, 4, true).unwrap();
        assert!(decision.effective_min_runners <= 2);
        assert!(decision.proposed_min_runners <= 3);
        assert_ne!(decision.status, DecisionStatus::AppliedWarmPoolFloor);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_predictive_warm_pool_is_bounded_by_operator_limit_and_global_max() {
        let dir = temp_dir("active-floor");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = false;
        settings.minimum_quality_score = 0.0;
        settings.minimum_samples = 24;
        settings.history_buckets = 48;
        settings.max_additional_warm_runners = 1;
        write_settings(&dir, &settings);

        let mut runtime = KittyRuntime::new(&dir);
        let mut last_decision = None;
        let start = 10_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        for step in 0..72u64 {
            let stamp = start + step * BUCKET_SECONDS;
            let decision = runtime.observe(input(stamp + 1, 1, 3), 2, 3, true).unwrap();
            runtime.finish_cycle(&decision, 0, true).unwrap();
            last_decision = Some(decision);
        }
        let decision = last_decision.unwrap();
        assert!(decision.effective_min_runners >= 2);
        assert!(decision.effective_min_runners <= 3);
        assert!(decision.proposed_min_runners <= 3);
        assert!(matches!(
            decision.status,
            DecisionStatus::AppliedWarmPoolFloor | DecisionStatus::RecommendationOnly
                | DecisionStatus::InsufficientHistory
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn observations_aggregate_bursts_and_remain_bounded() {
        let mut sample = Observation {
            bucket_epoch: 1_800,
            queued_jobs: 1,
            busy_runners: 2,
            online_runners: 2,
            managed_runners: 3,
            host_cpu_percent: Some(20.0),
            host_memory_percent: None,
            host_disk_percent: None,
            snapshots_in_bucket: 1,
        };
        let next = Observation {
            bucket_epoch: 1_800,
            queued_jobs: 4,
            busy_runners: 3,
            online_runners: 1,
            managed_runners: 2,
            host_cpu_percent: Some(40.0),
            host_memory_percent: Some(60.0),
            host_disk_percent: None,
            snapshots_in_bucket: 1,
        };
        sample.merge_snapshot(&next);
        assert_eq!(sample.queued_jobs, 4);
        assert_eq!(sample.busy_runners, 3);
        assert_eq!(sample.online_runners, 1);
        assert_eq!(sample.demand(), 7);
        assert_eq!(sample.snapshots_in_bucket, 2);
    }

    #[test]
    fn repository_key_is_stable_and_repository_path_is_validated() {
        assert_eq!(repository_key("owner/repo"), repository_key("owner/repo"));
        assert!(valid_repository("owner/repo"));
        assert!(!valid_repository("../outside"));
        assert!(!valid_repository("owner/repo/extra"));
        assert!(!valid_repository("owner/../secret"));
    }

    #[test]
    fn forecast_requires_sufficient_fresh_history_and_uses_only_observed_values() {
        let mut settings = enabled_settings();
        settings.minimum_samples = 24;
        settings.history_buckets = 48;
        let mut model = WorkflowModel::empty("owner/repo");
        let start = 50_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        for index in 0..24u64 {
            model.history.push(Observation {
                bucket_epoch: start + index * BUCKET_SECONDS,
                queued_jobs: 1,
                busy_runners: 2,
                online_runners: 3,
                managed_runners: 3,
                host_cpu_percent: Some(10.0),
                host_memory_percent: Some(20.0),
                host_disk_percent: Some(30.0),
                snapshots_in_bucket: 1,
            });
        }
        // Forecast is held back when no evaluated forecast quality exists.
        let forecast = compute_forecast(
            &model,
            &settings,
            start + 23 * BUCKET_SECONDS,
        )
        .unwrap();
        assert_eq!(forecast.predicted_peak_concurrency, 3);
        assert_eq!(forecast.confidence_score, 0.0);
        assert_eq!(forecast.evaluated_forecasts, 0);
    }

    #[test]
    fn invalid_model_identity_fails_validation() {
        let model = WorkflowModel::empty("another/repo");
        assert!(model.validate_for("owner/repo", 48).is_err());
    }
}
