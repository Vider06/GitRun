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
const MAX_DASHBOARD_REPOSITORIES: usize = 64;
const MAX_AUDIT_RECORDS: usize = 128;
const MAX_EVALUATED_FORECASTS: usize = 4032;
const FORECAST_HORIZON_SECONDS: u64 = 3600;
const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
const COMPACT_MONTH_SECONDS: u64 = WEEK_SECONDS * 4;
const COMPACT_YEAR_SECONDS: u64 = COMPACT_MONTH_SECONDS * 12;
const MAX_WEEKLY_SUMMARIES: usize = 4;
const MAX_MONTHLY_SUMMARIES: usize = 12;
const MAX_YEARLY_SUMMARIES: usize = 64;
const TEMPORAL_SLOT_COUNT: usize = 7 * 24;
const MIN_SEASONAL_OCCURRENCES: f64 = 4.0;
const DEFAULT_WARM_POOL_LEAD_SECONDS: u64 = 300;
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
    pub workflow_model_visible: bool,
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
    /// Keep an applied predictive floor briefly to reduce capacity oscillation.
    pub warm_pool_min_hold_seconds: u64,
    /// Prepare predicted schedule-specific capacity this long before its target
    /// time. This is a lead time, not the hold duration.
    pub warm_pool_lead_seconds: u64,
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
            workflow_model_visible: true,
            predictive_warm_pool_enabled: false,
            adaptive_resource_management_enabled: false,
            smart_fast_stop_enabled: false,
            shadow_mode: true,
            minimum_samples: 576,
            history_buckets: MAX_HISTORY_BUCKETS,
            max_additional_warm_runners: 2,
            warm_pool_min_hold_seconds: 900,
            warm_pool_lead_seconds: DEFAULT_WARM_POOL_LEAD_SECONDS,
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
        if self.adaptive_resource_management_enabled || self.smart_fast_stop_enabled {
            return Err(KittyError::Invalid(
                "Adaptive Resource Management and Smart Fast Stop automation are reserved and not supported by this version".into(),
            ));
        }
        if !(300..=3600).contains(&self.warm_pool_min_hold_seconds)
            || !self.warm_pool_min_hold_seconds.is_multiple_of(300)
        {
            return Err(KittyError::Invalid(
                "warm_pool_min_hold_seconds must be between 300 and 3600".into(),
            ));
        }
        if !(300..=3600).contains(&self.warm_pool_lead_seconds)
            || !self.warm_pool_lead_seconds.is_multiple_of(300)
        {
            return Err(KittyError::Invalid(
                "warm_pool_lead_seconds must be between 300 and 3600 and be a multiple of 300"
                    .into(),
            ));
        }
        if !self.minimum_quality_score.is_finite()
            || !(0.5..=1.0).contains(&self.minimum_quality_score)
        {
            return Err(KittyError::Invalid(
                "minimum_quality_score must be a finite number between 0.5 and 1".into(),
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
        if !self.bucket_epoch.is_multiple_of(BUCKET_SECONDS) {
            return Err(KittyError::Invalid(
                "observation timestamp is not aligned to a five-minute bucket".into(),
            ));
        }
        Ok(())
    }

    fn merge_snapshot(&mut self, next: &Self) {
        // Preserve the peak combined demand from one observed snapshot. Taking
        // queue and busy maxima independently could fabricate concurrency.
        if next.demand() > self.demand() {
            self.queued_jobs = next.queued_jobs;
            self.busy_runners = next.busy_runners;
        }
        self.online_runners = next.online_runners;
        self.managed_runners = next.managed_runners;
        self.host_cpu_percent = next.host_cpu_percent;
        self.host_memory_percent = next.host_memory_percent;
        self.host_disk_percent = next.host_disk_percent;
        self.snapshots_in_bucket = self.snapshots_in_bucket.saturating_add(1);
    }
}

/// One compact demand aggregate for a recurring weekday/hour slot.
/// Tuple serialization keeps old-history JSON substantially smaller.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemporalDemandSlot(
    pub u64, // five-minute observations
    pub u64, // sum of observed demand
    pub u32, // peak demand observed in this slot
    pub u32, // distinct weekly periods in which this slot was observed
);

/// Summary for one fixed-duration retention period. The 168 slots correspond
/// to Monday 00:00 through Sunday 23:00 in the scheduler host's local time.
/// Periods themselves use stable epoch-aligned 7-day, 4-week and 48-week spans.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeriodSummary {
    pub period_start_epoch: u64,
    pub period_end_epoch: u64,
    pub observation_count: u64,
    pub demand_sum: u64,
    pub peak_demand: u32,
    #[serde(default)]
    pub slots: Vec<TemporalDemandSlot>,
}

impl PeriodSummary {
    fn new(period_start_epoch: u64, period_seconds: u64) -> Self {
        Self {
            period_start_epoch,
            period_end_epoch: period_start_epoch.saturating_add(period_seconds),
            observation_count: 0,
            demand_sum: 0,
            peak_demand: 0,
            slots: vec![TemporalDemandSlot::default(); TEMPORAL_SLOT_COUNT],
        }
    }

    fn record(&mut self, observation: &Observation) {
        let demand = observation.demand();
        self.observation_count = self.observation_count.saturating_add(1);
        self.demand_sum = self.demand_sum.saturating_add(u64::from(demand));
        self.peak_demand = self.peak_demand.max(demand);
        let slot = &mut self.slots[weekday_hour_slot(observation.bucket_epoch)];
        if slot.0 == 0 {
            slot.3 = slot.3.saturating_add(1);
        }
        slot.0 = slot.0.saturating_add(1);
        slot.1 = slot.1.saturating_add(u64::from(demand));
        slot.2 = slot.2.max(demand);
    }

    /// Merge aggregate values without changing this summary's period bounds.
    fn merge_values(&mut self, other: &Self) {
        self.observation_count = self
            .observation_count
            .saturating_add(other.observation_count);
        self.demand_sum = self.demand_sum.saturating_add(other.demand_sum);
        self.peak_demand = self.peak_demand.max(other.peak_demand);
        if self.slots.len() != TEMPORAL_SLOT_COUNT {
            self.slots
                .resize(TEMPORAL_SLOT_COUNT, TemporalDemandSlot::default());
        }
        for (slot, other_slot) in self.slots.iter_mut().zip(&other.slots) {
            slot.0 = slot.0.saturating_add(other_slot.0);
            slot.1 = slot.1.saturating_add(other_slot.1);
            slot.2 = slot.2.max(other_slot.2);
            slot.3 = slot.3.saturating_add(other_slot.3);
        }
    }
}

fn default_forecast_horizon_seconds() -> u64 {
    FORECAST_HORIZON_SECONDS
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
    /// Seasonality-aware estimate for the same weekday/hour as the next
    /// configured warm-pool lead time. None means the repeated pattern is not
    /// sufficiently represented in retained history.
    #[serde(default)]
    pub seasonal_target_epoch: u64,
    #[serde(default)]
    pub seasonal_predicted_peak_concurrency: Option<u32>,
    #[serde(default)]
    pub bias_correction: Option<f64>,
    #[serde(default)]
    pub seasonal_bias_correction: Option<f64>,
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
    #[serde(default = "default_forecast_horizon_seconds")]
    pub horizon_seconds: u64,
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
    WarmPoolHold,
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
    /// Bounded four-week view after raw five-minute samples expire.
    #[serde(default)]
    pub weekly_summaries: Vec<PeriodSummary>,
    /// Four-week (28-day) compact periods retained for up to twelve periods.
    #[serde(default)]
    pub monthly_summaries: Vec<PeriodSummary>,
    /// Twelve compact periods (336 days) retained for up to 64 years.
    #[serde(default)]
    pub yearly_summaries: Vec<PeriodSummary>,
    /// Bounded aggregate for older years so storage remains capped indefinitely.
    #[serde(default)]
    pub lifetime_summary: Option<PeriodSummary>,
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
            weekly_summaries: Vec::new(),
            monthly_summaries: Vec::new(),
            yearly_summaries: Vec::new(),
            lifetime_summary: None,
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
            || self.weekly_summaries.len() > MAX_WEEKLY_SUMMARIES
            || self.monthly_summaries.len() > MAX_MONTHLY_SUMMARIES
            || self.yearly_summaries.len() > MAX_YEARLY_SUMMARIES
            || self.predictions.len() > MAX_EVALUATED_FORECASTS
            || self.decision_audit.len() > MAX_AUDIT_RECORDS
        {
            return Err(KittyError::Invalid(
                "stored model exceeds its bounded history limits".into(),
            ));
        }
        for (summaries, period_seconds) in [
            (&self.weekly_summaries, WEEK_SECONDS),
            (&self.monthly_summaries, COMPACT_MONTH_SECONDS),
            (&self.yearly_summaries, COMPACT_YEAR_SECONDS),
        ] {
            let mut previous_start = None;
            for summary in summaries {
                validate_period_summary(summary, Some(period_seconds))?;
                if previous_start.is_some_and(|start| start >= summary.period_start_epoch) {
                    return Err(KittyError::Invalid(
                        "stored retention summaries are not strictly chronological".into(),
                    ));
                }
                previous_start = Some(summary.period_start_epoch);
            }
        }
        if let Some(summary) = &self.lifetime_summary {
            validate_period_summary(summary, None)?;
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
        for prediction in &self.predictions {
            if prediction.issued_at_bucket % BUCKET_SECONDS != 0
                || !(BUCKET_SECONDS..=FORECAST_HORIZON_SECONDS)
                    .contains(&prediction.horizon_seconds)
                || !prediction.horizon_seconds.is_multiple_of(BUCKET_SECONDS)
                || prediction.target_bucket
                    != prediction
                        .issued_at_bucket
                        .saturating_add(prediction.horizon_seconds)
                || prediction.target_bucket % BUCKET_SECONDS != 0
                || (prediction.skipped_missing_observation && prediction.actual_demand.is_some())
            {
                return Err(KittyError::Invalid(
                    "stored prediction record has inconsistent timestamps or outcome flags".into(),
                ));
            }
        }
        if let Some(forecast) = &self.latest_forecast {
            let valid_score = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
            if forecast.model_version != self.model_version
                || forecast.horizon_seconds != FORECAST_HORIZON_SECONDS
                || forecast.target_epoch
                    != forecast
                        .generated_at_epoch
                        .saturating_add(FORECAST_HORIZON_SECONDS)
                || forecast.sample_count == 0
                || forecast.sample_count > self.history.len()
                || forecast.evaluated_forecasts > self.predictions.len()
                || !valid_score(forecast.quality_score)
                || !valid_score(forecast.confidence_score)
                || forecast
                    .mean_absolute_error
                    .is_some_and(|value| !value.is_finite() || value < 0.0)
                || forecast.seasonal_target_epoch < forecast.generated_at_epoch
                || forecast.seasonal_target_epoch
                    > forecast
                        .generated_at_epoch
                        .saturating_add(FORECAST_HORIZON_SECONDS)
                || forecast
                    .bias_correction
                    .is_some_and(|value| !value.is_finite())
                || forecast
                    .seasonal_bias_correction
                    .is_some_and(|value| !value.is_finite())
            {
                return Err(KittyError::Invalid(
                    "stored forecast record is outside supported bounds".into(),
                ));
            }
        }
        for audit in &self.decision_audit {
            if audit.bucket_epoch % BUCKET_SECONDS != 0
                || audit.model_version != self.model_version
                // Audit counts describe the history size at decision time; they
                // can legitimately exceed a later reduced retention setting.
                // The hard on-disk cap still rejects implausible/corrupt counts.
                || audit.sample_count > MAX_HISTORY_BUCKETS
                || audit
                    .source_window_start
                    .zip(audit.source_window_end)
                    .is_some_and(|(start, end)| start > end || end > audit.bucket_epoch)
                || audit.source_window_start.is_some() != audit.source_window_end.is_some()
                || audit
                    .confidence_score
                    .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                return Err(KittyError::Invalid(
                    "stored decision audit is outside supported bounds".into(),
                ));
            }
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
    pub fn fallback(
        repository: &str,
        configured_min_runners: u32,
        reason: impl Into<String>,
    ) -> Self {
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

    /// System installs expose only non-secret settings through /etc/gitrun;
    /// model history remains private under the configured state directory.
    pub fn uses_system_settings_path(state_dir: &Path) -> bool {
        trusted_system_state_dir().as_deref() == Some(state_dir)
    }

    pub fn settings_path(state_dir: &Path) -> PathBuf {
        if Self::uses_system_settings_path(state_dir) {
            PathBuf::from("/etc/gitrun/kitty.json")
        } else {
            state_dir.join(SETTINGS_RELATIVE_PATH)
        }
    }

    pub fn dashboard_status_path(state_dir: &Path) -> PathBuf {
        if Self::uses_system_settings_path(state_dir) {
            PathBuf::from("/etc/gitrun/kitty-status.json")
        } else {
            state_dir.join("kitty/status.json")
        }
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
                "repository model failed validation earlier; repair or remove its model file and restart gitrun-autoscaler to re-enable Kitty",
            ));
        }
        if !self.models.contains_key(&key) {
            if self.models.len() >= MAX_MODELS {
                if let Err(error) = self.evict_oldest_model() {
                    return Ok(KittyDecision::fallback(
                        &input.repository,
                        configured_min_runners,
                        format!("Kitty could not safely evict an older model; deterministic scheduler unchanged: {error}"),
                    ));
                }
            }
            match self.load_model(&input.repository, &key) {
                Ok(model) => {
                    self.models.insert(key.clone(), model);
                }
                Err(error) => {
                    if self.disabled_models.len() < MAX_MODELS {
                        self.disabled_models.insert(key.clone());
                    }
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
        if compact_model_history(
            model,
            input.observed_at_epoch,
            self.settings.history_buckets,
        ) {
            model.latest_forecast = None;
            self.dirty_models.insert(key.clone());
        }
        model.validate_for(&input.repository, self.settings.history_buckets)?;
        if model
            .history
            .last()
            .is_some_and(|last| bucket < last.bucket_epoch)
        {
            return Ok(KittyDecision::fallback(
                &input.repository,
                configured_min_runners,
                "system clock moved backwards; predictive action withheld",
            ));
        }

        // Use this timestamp for in-memory recency as well as the persisted
        // source-window metadata. Otherwise an evicted/reloaded repository can
        // remain "oldest" forever when its samples are merged into the same
        // five-minute bucket.
        model.generated_at_epoch = input.observed_at_epoch;
        let (demand_peak_increased, new_bucket) = if let Some(last) = model
            .history
            .last_mut()
            .filter(|last| last.bucket_epoch == bucket)
        {
            let old_peak = last.demand();
            last.merge_snapshot(&observation);
            (last.demand() > old_peak, false)
        } else {
            model.history.push(observation);
            evaluate_matured_predictions(model, bucket);
            if compact_model_history(
                model,
                input.observed_at_epoch,
                self.settings.history_buckets,
            ) {
                model.latest_forecast = None;
                self.dirty_models.insert(key.clone());
            }
            if self.settings.predictive_analytics_enabled {
                model.latest_forecast =
                    compute_forecast(model, &self.settings, input.observed_at_epoch);
                if let Some(forecast) = &model.latest_forecast {
                    model.predictions.push(PredictionRecord {
                        issued_at_bucket: bucket,
                        target_bucket: bucket.saturating_add(FORECAST_HORIZON_SECONDS),
                        horizon_seconds: FORECAST_HORIZON_SECONDS,
                        predicted_demand: forecast.predicted_peak_concurrency,
                        actual_demand: None,
                        skipped_missing_observation: false,
                    });
                    if let Some(seasonal_prediction) = forecast.seasonal_predicted_peak_concurrency
                    {
                        model.predictions.push(PredictionRecord {
                            issued_at_bucket: bucket,
                            target_bucket: bucket
                                .saturating_add(self.settings.warm_pool_lead_seconds),
                            horizon_seconds: self.settings.warm_pool_lead_seconds,
                            predicted_demand: seasonal_prediction,
                            actual_demand: None,
                            skipped_missing_observation: false,
                        });
                    }
                }
            } else {
                model.latest_forecast = None;
            }
            if model.predictions.len() > MAX_EVALUATED_FORECASTS {
                let excess = model.predictions.len() - MAX_EVALUATED_FORECASTS;
                model.predictions.drain(0..excess);
            }
            (true, true)
        };
        if new_bucket || demand_peak_increased {
            // Persist a newly observed peak in the current bucket. Without
            // this, a restart before the next bucket could lose the queue
            // burst that the forecast history is intended to preserve.
            self.dirty_models.insert(key.clone());
        }

        let forecast = if self.settings.predictive_analytics_enabled {
            model.latest_forecast.as_ref().filter(|forecast| {
                input
                    .observed_at_epoch
                    .saturating_sub(forecast.generated_at_epoch)
                    <= BUCKET_SECONDS.saturating_mul(2)
            })
        } else {
            None
        };
        let sample_count = model.history.len();
        let forecast_demand = forecast.map(|f| {
            f.seasonal_predicted_peak_concurrency
                .unwrap_or(f.predicted_peak_concurrency)
        });
        let proposed_min = forecast
            .filter(|f| {
                self.settings.predictive_warm_pool_enabled
                    && f.confidence_score >= self.settings.minimum_quality_score
            })
            .map(|f| {
                let desired = f
                    .seasonal_predicted_peak_concurrency
                    .unwrap_or(f.predicted_peak_concurrency);
                let additional = desired
                    .saturating_sub(configured_min_runners)
                    .min(self.settings.max_additional_warm_runners);
                configured_min_runners
                    .saturating_add(additional)
                    .min(max_runners)
                    .max(configured_min_runners.min(max_runners))
            })
            .unwrap_or(configured_min_runners.min(max_runners));

        let held_floor = if self.settings.predictive_warm_pool_enabled
            && !self.settings.shadow_mode
            && capacity_growth_allowed
        {
            retained_warm_pool_floor(
                model,
                bucket,
                configured_min_runners,
                max_runners,
                self.settings.max_additional_warm_runners,
                self.settings.warm_pool_min_hold_seconds,
            )
        } else {
            None
        };

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
            if let Some(held) = held_floor.filter(|floor| *floor > configured_min_runners) {
                (
                    DecisionStatus::WarmPoolHold,
                    held,
                    format!(
                        "No fresh actionable forecast is available; retaining the previously approved warm-pool floor at {} until its bounded hold expires.",
                        held
                    ),
                )
            } else {
                (
                    DecisionStatus::InsufficientHistory,
                    configured_min_runners,
                    format!(
                        "Collecting bounded five-minute observations; {} samples are available and {} are required. No predictive action is permitted.",
                        sample_count, self.settings.minimum_samples
                    ),
                )
            }
        } else {
            let f = forecast.expect("checked forecast");
            let acceptable = f.confidence_score >= self.settings.minimum_quality_score
                && sample_count >= self.settings.minimum_samples;
            if !acceptable {
                if let Some(held) = held_floor.filter(|floor| *floor > configured_min_runners) {
                    (
                        DecisionStatus::WarmPoolHold,
                        held,
                        format!(
                            "Forecast readiness {:.3} is below the configured threshold {:.3}; retaining the previously approved warm-pool floor at {} only until its bounded hold expires.",
                            f.confidence_score, self.settings.minimum_quality_score, held
                        ),
                    )
                } else {
                    (
                        DecisionStatus::InsufficientHistory,
                        configured_min_runners,
                        format!(
                            "Forecast readiness {:.3} is below the configured threshold {:.3}; no predictive action.",
                            f.confidence_score, self.settings.minimum_quality_score
                        ),
                    )
                }
            } else if !self.settings.predictive_warm_pool_enabled {
                (
                    DecisionStatus::RecommendationOnly,
                    configured_min_runners,
                    format!(
                        "Forecast demand is {}; capacity automation is disabled.",
                        f.seasonal_predicted_peak_concurrency
                            .unwrap_or(f.predicted_peak_concurrency)
                    ),
                )
            } else if self.settings.shadow_mode {
                (
                    DecisionStatus::ShadowRecommendation,
                    configured_min_runners,
                    format!(
                        "Shadow recommendation: raise the runner floor from {} to {} for the one-hour demand forecast. No capacity change applied.",
                        configured_min_runners,
                        proposed_min
                    ),
                )
            } else if !capacity_growth_allowed {
                (
                    DecisionStatus::ResourcePressureHold,
                    configured_min_runners,
                    "Forecast prewarming withheld by existing scheduler resource-pressure backpressure.".to_owned(),
                )
            } else {
                let held = held_floor.unwrap_or(configured_min_runners.min(max_runners));
                let target = proposed_min.max(held).min(max_runners);
                if target > proposed_min && proposed_min <= configured_min_runners {
                    (
                        DecisionStatus::WarmPoolHold,
                        target,
                        format!(
                            "Retaining the previously applied warm-pool floor at {} for up to {} seconds to reduce oscillation; current observed forecast no longer requires the higher floor.",
                            target, self.settings.warm_pool_min_hold_seconds
                        ),
                    )
                } else if target > configured_min_runners {
                    (
                        DecisionStatus::AppliedWarmPoolFloor,
                        target,
                        format!(
                            "Predictive warm-pool floor raised/retained within explicit bounds: {} to {} (forecast {}, readiness {:.3}, hold {}s).",
                            configured_min_runners,
                            target,
                            f.seasonal_predicted_peak_concurrency
                                .unwrap_or(f.predicted_peak_concurrency),
                            f.confidence_score,
                            self.settings.warm_pool_min_hold_seconds
                        ),
                    )
                } else {
                    (
                        DecisionStatus::RecommendationOnly,
                        configured_min_runners,
                        format!(
                            "Forecast demand is {}; no prewarm is required above the deterministic floor.",
                            f.predicted_peak_concurrency
                        ),
                    )
                }
            }
        };

        let mut decision = KittyDecision {
            repository: input.repository.clone(),
            status,
            reason: format!("{reason}{capability_note}"),
            configured_min_runners,
            proposed_min_runners: proposed_min,
            effective_min_runners: effective_min,
            predicted_peak_concurrency: forecast_demand,
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
                    // Record a subsequent retry separately after an action
                    // failed; a later successful reconcile must not be hidden
                    // by the failed outcome of the first attempt.
                    || last.execution_outcome.as_deref() == Some("action_failure")
            })
            .unwrap_or(true);
        if should_add_audit {
            if matches!(
                decision.status,
                DecisionStatus::ShadowRecommendation
                    | DecisionStatus::AppliedWarmPoolFloor
                    | DecisionStatus::WarmPoolHold
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
            self.dirty_models.insert(key.clone());
        }

        // A raised floor is not passed to the planner until its source model and
        // decision audit have been persisted successfully. Failure fails closed.
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
            // Dashboard status is an informational, best-effort projection;
            // failure to refresh it must never disable deterministic scheduling.
            if let Err(error) = self.persist_dashboard_snapshot() {
                eprintln!("gitrun-smart-scheduler: dashboard status persistence failed: {error}");
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
                // Validate against the hard storage bound first. The active
                // operator history window is applied in observe(), where the
                // truncation is marked dirty and then durably saved.
                model.validate_for(repository, MAX_HISTORY_BUCKETS)?;
                model.history.shrink_to_fit();
                model.predictions.shrink_to_fit();
                Ok(model)
            }
        }
    }

    fn persist_model(&self, key: &str) -> Result<()> {
        let model = self
            .models
            .get(key)
            .ok_or_else(|| KittyError::Invalid("attempted to persist an absent model".into()))?;
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

    /// Keep memory bounded while allowing repositories beyond the cache
    /// limit to build independent persistent models. Dirty history is made
    /// durable before an entry can be evicted.
    fn evict_oldest_model(&mut self) -> Result<()> {
        if self.models.len() < MAX_MODELS {
            return Ok(());
        }
        let victim = self
            .models
            .iter()
            .min_by_key(|(_, model)| model.generated_at_epoch)
            .map(|(key, _)| key.clone())
            .ok_or(KittyError::ModelLimit)?;
        if self.dirty_models.contains(&victim) {
            self.persist_model(&victim)?;
            self.dirty_models.remove(&victim);
            if let Err(error) = self.persist_dashboard_snapshot() {
                eprintln!(
                    "gitrun-smart-scheduler: dashboard status persistence failed during model eviction: {error}"
                );
            }
        }
        self.models.remove(&victim).ok_or(KittyError::ModelLimit)?;
        Ok(())
    }

    fn dashboard_snapshot(&self) -> KittyDashboardStatus {
        let mut repositories: Vec<_> = self
            .models
            .values()
            .map(|model| KittyRepositoryStatus {
                repository: model.repository.clone(),
                sample_count: model.history.len(),
                first_observation_epoch: model.history.first().map(|entry| entry.bucket_epoch),
                latest_observation_epoch: model.history.last().map(|entry| entry.bucket_epoch),
                latest_forecast: model.latest_forecast.clone(),
                latest_decision: model.decision_audit.last().cloned(),
            })
            .collect();
        repositories.sort_by(|left, right| left.repository.cmp(&right.repository));
        KittyDashboardStatus {
            schema_version: SETTINGS_SCHEMA_VERSION,
            generated_at_epoch: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            enabled: self.settings.enabled,
            telemetry_enabled: self.settings.telemetry_enabled,
            predictive_analytics_enabled: self.settings.predictive_analytics_enabled,
            workflow_model_visible: self.settings.workflow_model_visible,
            predictive_warm_pool_enabled: self.settings.predictive_warm_pool_enabled,
            shadow_mode: self.settings.shadow_mode,
            repositories,
        }
    }

    fn persist_dashboard_snapshot(&self) -> Result<()> {
        let mut status = self.dashboard_snapshot();
        // Preserve the last known summary for models temporarily evicted from
        // the bounded in-memory cache. Only the 64 most recently observed
        // repositories are exposed; full history remains in private model files.
        if let Ok(previous) = load_dashboard_status(&self.state_dir) {
            let mut repositories: HashMap<String, KittyRepositoryStatus> = previous
                .repositories
                .into_iter()
                .map(|entry| (entry.repository.clone(), entry))
                .collect();
            for entry in status.repositories.drain(..) {
                repositories.insert(entry.repository.clone(), entry);
            }
            let now = status.generated_at_epoch;
            let cutoff =
                now.saturating_sub((MAX_HISTORY_BUCKETS as u64).saturating_mul(BUCKET_SECONDS));
            let mut recent: Vec<_> = repositories
                .into_values()
                .filter(|entry| {
                    entry
                        .latest_observation_epoch
                        .is_some_and(|stamp| stamp <= now && stamp >= cutoff)
                })
                .collect();
            recent.sort_by_key(|entry| {
                std::cmp::Reverse(entry.latest_observation_epoch.unwrap_or_default())
            });
            recent.truncate(MAX_DASHBOARD_REPOSITORIES);
            recent.sort_by(|left, right| left.repository.cmp(&right.repository));
            status.repositories = recent;
        }
        let bytes = serde_json::to_vec(&status)?;
        if bytes.len() as u64 > MAX_MODEL_BYTES {
            return Err(KittyError::Invalid(
                "dashboard status snapshot exceeded the size limit".into(),
            ));
        }
        let path = Self::dashboard_status_path(&self.state_dir);
        if Self::uses_system_settings_path(&self.state_dir) {
            let group = system_settings_group(&Self::settings_path(&self.state_dir))?;
            atomic_write_system_readable(&path, &bytes, group)?;
        } else {
            atomic_write_private(&path, &bytes)?;
        }
        Ok(())
    }
}

/// Privacy-minimized model status suitable for the operator dashboard. Raw
/// five-minute history and per-forecast records never leave the private files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KittyRepositoryStatus {
    pub repository: String,
    pub sample_count: usize,
    pub first_observation_epoch: Option<u64>,
    pub latest_observation_epoch: Option<u64>,
    pub latest_forecast: Option<Forecast>,
    pub latest_decision: Option<DecisionAudit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KittyDashboardStatus {
    pub schema_version: u32,
    pub generated_at_epoch: u64,
    pub enabled: bool,
    pub telemetry_enabled: bool,
    pub predictive_analytics_enabled: bool,
    pub workflow_model_visible: bool,
    pub predictive_warm_pool_enabled: bool,
    pub shadow_mode: bool,
    pub repositories: Vec<KittyRepositoryStatus>,
}

impl KittyDashboardStatus {
    fn empty(settings: &KittySettings) -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            generated_at_epoch: 0,
            enabled: settings.enabled,
            telemetry_enabled: settings.telemetry_enabled,
            predictive_analytics_enabled: settings.predictive_analytics_enabled,
            workflow_model_visible: settings.workflow_model_visible,
            predictive_warm_pool_enabled: settings.predictive_warm_pool_enabled,
            shadow_mode: settings.shadow_mode,
            repositories: Vec::new(),
        }
    }
}

pub fn load_settings_for_dashboard(state_dir: &Path) -> Result<KittySettings> {
    let path = KittyRuntime::settings_path(state_dir);
    if KittyRuntime::uses_system_settings_path(state_dir) {
        let settings =
            read_system_json::<KittySettings>(&path, MAX_SETTINGS_BYTES)?.unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    } else {
        read_settings(&path)
    }
}

pub fn save_settings_for_local_state_dir(state_dir: &Path, settings: &KittySettings) -> Result<()> {
    settings.validate()?;
    if KittyRuntime::uses_system_settings_path(state_dir) {
        return Err(KittyError::Invalid(
            "system settings must be saved through the privileged GitRun helper".into(),
        ));
    }
    let bytes = serde_json::to_vec(settings)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(KittyError::Invalid(
            "Kitty settings exceed the size limit".into(),
        ));
    }
    atomic_write_private(&KittyRuntime::settings_path(state_dir), &bytes)
}

/// Root-only writer called by the authenticated internal CLI command. The CLI
/// then grants the desktop user's primary group read-only access.
pub fn load_dashboard_status(state_dir: &Path) -> Result<KittyDashboardStatus> {
    let settings = load_settings_for_dashboard(state_dir)?;
    let path = KittyRuntime::dashboard_status_path(state_dir);
    let stored = if KittyRuntime::uses_system_settings_path(state_dir) {
        read_system_json::<KittyDashboardStatus>(&path, MAX_MODEL_BYTES)?
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| KittyError::Invalid("status path has no parent directory".into()))?;
        if !validate_read_data_dir(parent)? {
            None
        } else {
            read_checked(&path, MAX_MODEL_BYTES)?
                .map(|bytes| serde_json::from_slice(&bytes))
                .transpose()?
        }
    };
    let mut status = stored.unwrap_or_else(|| KittyDashboardStatus::empty(&settings));
    status.enabled = settings.enabled;
    status.telemetry_enabled = settings.telemetry_enabled;
    status.predictive_analytics_enabled = settings.predictive_analytics_enabled;
    status.workflow_model_visible = settings.workflow_model_visible;
    status.predictive_warm_pool_enabled = settings.predictive_warm_pool_enabled;
    status.shadow_mode = settings.shadow_mode;
    if !settings.workflow_model_visible {
        status.repositories.clear();
    }
    Ok(status)
}

fn read_settings(path: &Path) -> Result<KittySettings> {
    if is_system_settings_path(path) {
        let settings =
            read_system_json::<KittySettings>(path, MAX_SETTINGS_BYTES)?.unwrap_or_default();
        settings.validate()?;
        return Ok(settings);
    }

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

fn is_system_settings_path(path: &Path) -> bool {
    path == Path::new("/etc/gitrun/kitty.json")
}

fn trusted_system_state_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = Path::new("/etc/gitrun");
        let config_path = parent.join("gitrun.env");
        let parent_meta = fs::symlink_metadata(parent).ok()?;
        let config_meta = fs::symlink_metadata(&config_path).ok()?;
        if parent_meta.file_type().is_symlink()
            || !parent_meta.is_dir()
            || parent_meta.uid() != 0
            || parent_meta.mode() & 0o022 != 0
            || config_meta.file_type().is_symlink()
            || !config_meta.is_file()
            || config_meta.uid() != 0
            || config_meta.nlink() != 1
            || config_meta.mode() & 0o022 != 0
            || config_meta.len() > 1024 * 1024
        {
            return None;
        }
        let content = fs::read_to_string(config_path).ok()?;
        let mut state_dir = PathBuf::from("/var/lib/gitrun");
        for line in content.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "GITRUN_STATE_DIR" {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if value.is_empty() {
                    return None;
                }
                state_dir = PathBuf::from(value);
            }
        }
        Some(state_dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn system_settings_group(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(KittyError::Invalid(
            "system Kitty settings are supported only on Unix".into(),
        ))
    }
}

fn validate_system_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = path
            .parent()
            .ok_or_else(|| KittyError::Invalid("system Kitty file has no parent".into()))?;
        if parent != Path::new("/etc/gitrun") {
            return Err(KittyError::Invalid(
                "system Kitty files must live directly under /etc/gitrun".into(),
            ));
        }
        for directory in [Path::new("/etc"), parent] {
            let metadata = fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.mode() & 0o022 != 0
            {
                return Err(KittyError::Invalid(format!(
                    "{} is not a trusted root-owned directory",
                    directory.display()
                )));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(KittyError::Invalid(
            "system Kitty settings are supported only on Unix".into(),
        ))
    }
}

fn read_system_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<T>> {
    let Some(bytes) = read_system_checked(path, max_bytes)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(unix)]
fn read_system_checked(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    validate_system_parent(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o640
        || metadata.len() > max_bytes
    {
        return Err(KittyError::Invalid(format!(
            "{} must be a bounded, root-owned regular file with no links or group/world writes",
            path.display()
        )));
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.uid() != 0
        || opened.nlink() != 1
        || opened.mode() & 0o777 != 0o640
        || opened.len() > max_bytes
    {
        return Err(KittyError::Invalid(format!(
            "{} changed during system-file validation",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(opened.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(KittyError::Invalid(format!(
            "{} exceeded its read-size bound",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

#[cfg(not(unix))]
fn read_system_checked(_path: &Path, _max_bytes: u64) -> Result<Option<Vec<u8>>> {
    Err(KittyError::Invalid(
        "system Kitty settings are supported only on Unix".into(),
    ))
}

#[cfg(unix)]
fn atomic_write_system_readable(path: &Path, bytes: &[u8], group: u32) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    if unsafe { libc::geteuid() } != 0 {
        return Err(KittyError::Invalid(
            "system Kitty status persistence requires root privileges".into(),
        ));
    }
    validate_system_parent(path)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != 0
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
        {
            return Err(KittyError::Invalid(format!(
                "{} is an unsafe existing status file",
                path.display()
            )));
        }
    }
    let parent = path.parent().expect("validated parent");
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".kitty-status-tmp-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        if unsafe { libc::fchown(file.as_raw_fd(), 0, group) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o640))?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(unix))]
fn atomic_write_system_readable(_path: &Path, _bytes: &[u8], _group: u32) -> Result<()> {
    Err(KittyError::Invalid(
        "system Kitty status persistence is supported only on Unix".into(),
    ))
}

fn validate_period_summary(
    summary: &PeriodSummary,
    expected_period_seconds: Option<u64>,
) -> Result<()> {
    if summary.period_end_epoch <= summary.period_start_epoch
        || summary.slots.len() != TEMPORAL_SLOT_COUNT
        || expected_period_seconds.is_some_and(|seconds| {
            summary
                .period_end_epoch
                .saturating_sub(summary.period_start_epoch)
                != seconds
                || summary.period_start_epoch % seconds != 0
        })
    {
        return Err(KittyError::Invalid(
            "stored retention summary has invalid period bounds or time-slot count".into(),
        ));
    }
    Ok(())
}

fn utc_weekday_hour_slot(epoch: u64) -> usize {
    let days = epoch / 86_400;
    let weekday = ((days + 3) % 7) as usize; // Monday = 0; 1970-01-01 was Thursday.
    let hour = ((epoch % 86_400) / 3_600) as usize;
    weekday * 24 + hour
}

fn weekday_hour_slot(epoch: u64) -> usize {
    #[cfg(unix)]
    {
        let Ok(timestamp) = libc::time_t::try_from(epoch) else {
            return utc_weekday_hour_slot(epoch);
        };
        let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
        let result = unsafe { libc::localtime_r(&timestamp, &mut local) };
        if !result.is_null() && (0..24).contains(&local.tm_hour) {
            let weekday = (local.tm_wday + 6).rem_euclid(7) as usize;
            return weekday * 24 + local.tm_hour as usize;
        }
    }
    utc_weekday_hour_slot(epoch)
}

fn weekly_period_start(epoch: u64) -> u64 {
    let days = epoch / 86_400;
    let weekday = (days + 3) % 7; // Monday-aligned UTC week.
    days.saturating_sub(weekday).saturating_mul(86_400)
}

fn compact_model_history(model: &mut WorkflowModel, now_epoch: u64, history_limit: usize) -> bool {
    let history_limit = history_limit.clamp(24, MAX_HISTORY_BUCKETS);
    let raw_window_seconds = (history_limit as u64).saturating_mul(BUCKET_SECONDS);
    let cutoff = now_epoch.saturating_sub(raw_window_seconds);
    let expired = model
        .history
        .iter()
        .take_while(|sample| sample.bucket_epoch < cutoff)
        .count();
    let excess = model.history.len().saturating_sub(history_limit);
    let remove_count = expired.max(excess);
    let mut changed = false;

    if remove_count > 0 {
        let removed: Vec<_> = model.history.drain(0..remove_count).collect();
        for sample in removed {
            let start = weekly_period_start(sample.bucket_epoch);
            let summary = if let Some(index) = model
                .weekly_summaries
                .iter()
                .position(|entry| entry.period_start_epoch == start)
            {
                &mut model.weekly_summaries[index]
            } else {
                model
                    .weekly_summaries
                    .push(PeriodSummary::new(start, WEEK_SECONDS));
                model
                    .weekly_summaries
                    .sort_by_key(|entry| entry.period_start_epoch);
                let index = model
                    .weekly_summaries
                    .iter()
                    .position(|entry| entry.period_start_epoch == start)
                    .expect("new weekly summary was inserted");
                &mut model.weekly_summaries[index]
            };
            summary.record(&sample);
        }
        changed = true;
    }

    // A complete rolling 4-week period is folded only after its end has left
    // the configured high-resolution window. This avoids double-counting a
    // period whose most recent observations are still in raw history.
    let month_starts: HashSet<u64> = model
        .weekly_summaries
        .iter()
        .map(|entry| entry.period_start_epoch / COMPACT_MONTH_SECONDS * COMPACT_MONTH_SECONDS)
        .filter(|start| start.saturating_add(COMPACT_MONTH_SECONDS) <= cutoff)
        .collect();
    for start in month_starts {
        let end = start.saturating_add(COMPACT_MONTH_SECONDS);
        let mut summary = PeriodSummary::new(start, COMPACT_MONTH_SECONDS);
        let mut retained = Vec::with_capacity(model.weekly_summaries.len());
        let mut found = false;
        for entry in model.weekly_summaries.drain(..) {
            if entry.period_start_epoch >= start && entry.period_start_epoch < end {
                summary.merge_values(&entry);
                found = true;
            } else {
                retained.push(entry);
            }
        }
        model.weekly_summaries = retained;
        if found {
            if let Some(existing) = model
                .monthly_summaries
                .iter_mut()
                .find(|entry| entry.period_start_epoch == start)
            {
                existing.merge_values(&summary);
            } else {
                model.monthly_summaries.push(summary);
            }
            model
                .monthly_summaries
                .sort_by_key(|entry| entry.period_start_epoch);
            changed = true;
        }
    }

    // A rolling 48-week year is folded only when its full retention period is
    // past the cutoff; the newest year therefore remains represented by up to
    // twelve monthly summaries rather than an ever-growing five-minute log.
    let year_starts: HashSet<u64> = model
        .monthly_summaries
        .iter()
        .map(|entry| entry.period_start_epoch / COMPACT_YEAR_SECONDS * COMPACT_YEAR_SECONDS)
        .filter(|start| start.saturating_add(COMPACT_YEAR_SECONDS) <= cutoff)
        .collect();
    for start in year_starts {
        let end = start.saturating_add(COMPACT_YEAR_SECONDS);
        let mut summary = PeriodSummary::new(start, COMPACT_YEAR_SECONDS);
        let mut retained = Vec::with_capacity(model.monthly_summaries.len());
        let mut found = false;
        for entry in model.monthly_summaries.drain(..) {
            if entry.period_start_epoch >= start && entry.period_start_epoch < end {
                summary.merge_values(&entry);
                found = true;
            } else {
                retained.push(entry);
            }
        }
        model.monthly_summaries = retained;
        if found {
            model.yearly_summaries.push(summary);
            model
                .yearly_summaries
                .sort_by_key(|entry| entry.period_start_epoch);
            changed = true;
        }
    }

    // Keep the last 64 annual summaries and fold still older years into one
    // lifetime profile. Storage therefore remains bounded for arbitrarily old
    // installations while retaining long-term weekday/hour statistics.
    while model.yearly_summaries.len() > MAX_YEARLY_SUMMARIES {
        let oldest = model.yearly_summaries.remove(0);
        if let Some(lifetime) = model.lifetime_summary.as_mut() {
            lifetime.period_start_epoch =
                lifetime.period_start_epoch.min(oldest.period_start_epoch);
            lifetime.period_end_epoch = lifetime.period_end_epoch.max(oldest.period_end_epoch);
            lifetime.merge_values(&oldest);
        } else {
            model.lifetime_summary = Some(oldest);
        }
        changed = true;
    }

    changed
}

#[derive(Default)]
struct SeasonalSlotStats {
    weighted_samples: f64,
    weighted_demand_sum: f64,
    occurrences: f64,
}

fn seasonal_slot_stats(
    model: &WorkflowModel,
    slot_index: usize,
    now_epoch: u64,
) -> SeasonalSlotStats {
    let mut stats = SeasonalSlotStats::default();
    let mut observed_weeks = HashSet::new();

    for observation in &model.history {
        if weekday_hour_slot(observation.bucket_epoch) == slot_index {
            stats.weighted_samples += 1.0;
            stats.weighted_demand_sum += f64::from(observation.demand());
            observed_weeks.insert(weekly_period_start(observation.bucket_epoch));
        }
    }
    stats.occurrences += observed_weeks.len() as f64;

    let mut add_summary = |summary: &PeriodSummary, half_life_seconds: f64| {
        if let Some(slot) = summary.slots.get(slot_index) {
            if slot.0 == 0 {
                return;
            }
            let age = now_epoch.saturating_sub(summary.period_end_epoch) as f64;
            let weight = 2.0_f64.powf(-age / half_life_seconds.max(1.0));
            stats.weighted_samples += slot.0 as f64 * weight;
            stats.weighted_demand_sum += slot.1 as f64 * weight;
            stats.occurrences += slot.3 as f64 * weight;
        }
    };

    for summary in &model.weekly_summaries {
        add_summary(summary, (30 * 86_400) as f64);
    }
    for summary in &model.monthly_summaries {
        add_summary(summary, (180 * 86_400) as f64);
    }
    for summary in &model.yearly_summaries {
        add_summary(summary, (3 * 365 * 86_400) as f64);
    }
    if let Some(summary) = &model.lifetime_summary {
        add_summary(summary, (10 * 365 * 86_400) as f64);
    }

    stats
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

    let demands: Vec<f64> = history
        .iter()
        .map(|observation| observation.demand() as f64)
        .collect();
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
    let recent_p90 = percentile(recent, 0.90);
    let mut ewma = recent[0];
    for value in recent.iter().skip(1) {
        ewma = 0.35 * value + 0.65 * ewma;
    }
    // Keep the forecast peak-sensitive without allowing one outlier to dominate:
    // the recent 90th percentile is a floor, combined with EWMA and short trend.
    let corrected = ewma + trend + bias_correction.unwrap_or(0.0);
    let predicted = corrected.max(recent_mean).max(recent_p90).max(0.0).ceil();
    let predicted_peak_concurrency = predicted.min(u32::MAX as f64) as u32;

    let seasonal_target_epoch = now_epoch.saturating_add(settings.warm_pool_lead_seconds);
    let seasonal_slot = weekday_hour_slot(seasonal_target_epoch);
    let seasonal_stats = seasonal_slot_stats(model, seasonal_slot, now_epoch);
    let seasonal_occurrences = seasonal_stats.occurrences;
    let seasonal_prediction = if seasonal_occurrences >= MIN_SEASONAL_OCCURRENCES
        && seasonal_stats.weighted_samples > 0.0
    {
        let seasonal_mean = seasonal_stats.weighted_demand_sum / seasonal_stats.weighted_samples;
        let schedule_correction = model
            .predictions
            .iter()
            .filter(|record| {
                record.horizon_seconds == settings.warm_pool_lead_seconds
                    && record.actual_demand.is_some()
                    && !record.skipped_missing_observation
            })
            .rev()
            .take(48)
            .filter_map(|record| {
                record
                    .actual_demand
                    .map(|actual| f64::from(actual) - f64::from(record.predicted_demand))
            })
            .collect::<Vec<_>>();
        let seasonal_bias_correction =
            (schedule_correction.len() >= 4).then(|| mean(&schedule_correction));
        let corrected = (seasonal_mean + seasonal_bias_correction.unwrap_or(0.0))
            .max(0.0)
            .ceil()
            .min(u32::MAX as f64) as u32;
        Some((corrected, seasonal_bias_correction))
    } else {
        None
    };

    let evaluated: Vec<&PredictionRecord> = model
        .predictions
        .iter()
        .filter(|record| {
            record.horizon_seconds == FORECAST_HORIZON_SECONDS
                && record.actual_demand.is_some()
                && !record.skipped_missing_observation
        })
        .rev()
        .take(288)
        .collect();
    let correction_samples: Vec<f64> = evaluated
        .iter()
        .take(48)
        .filter_map(|record| {
            record
                .actual_demand
                .map(|actual| f64::from(actual) - f64::from(record.predicted_demand))
        })
        .collect();
    let bias_correction = (correction_samples.len() >= 8).then(|| mean(&correction_samples));
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
    let freshness_factor =
        (1.0 - age as f64 / (BUCKET_SECONDS.saturating_mul(3) as f64)).clamp(0.0, 1.0);
    let confidence_score =
        (sample_factor * evaluation_factor * quality_score * freshness_factor).clamp(0.0, 1.0);

    let (seasonal_predicted_peak_concurrency, seasonal_bias_correction) = seasonal_prediction
        .map(|(prediction, correction)| (Some(prediction), correction))
        .unwrap_or((None, None));
    Some(Forecast {
        model_version: MODEL_VERSION,
        generated_at_epoch: now_epoch,
        target_epoch: now_epoch.saturating_add(FORECAST_HORIZON_SECONDS),
        horizon_seconds: FORECAST_HORIZON_SECONDS,
        sample_count: history.len(),
        evaluated_forecasts: evaluated.len(),
        predicted_peak_concurrency,
        seasonal_target_epoch,
        seasonal_predicted_peak_concurrency,
        bias_correction,
        seasonal_bias_correction,
        mean_absolute_error: mae,
        quality_score,
        confidence_score,
    })
}

fn retained_warm_pool_floor(
    model: &WorkflowModel,
    current_bucket: u64,
    configured_min_runners: u32,
    max_runners: u32,
    max_additional_warm_runners: u32,
    hold_seconds: u64,
) -> Option<u32> {
    let latest = model.decision_audit.iter().rev().find(|audit| {
        audit.status == DecisionStatus::AppliedWarmPoolFloor
            && audit.effective_min_runners > audit.configured_min_runners
    })?;
    if latest.model_version != model.model_version
        || latest.configured_min_runners != configured_min_runners
        || current_bucket < latest.bucket_epoch
        || current_bucket.saturating_sub(latest.bucket_epoch) > hold_seconds
    {
        return None;
    }
    let current_operator_cap = configured_min_runners
        .saturating_add(max_additional_warm_runners)
        .min(max_runners);
    let previous = latest
        .effective_min_runners
        .min(max_runners)
        .min(current_operator_cap);
    (previous > configured_min_runners).then_some(previous)
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
        } else if current_bucket > prediction.target_bucket.saturating_add(BUCKET_SECONDS) {
            prediction.skipped_missing_observation = true;
        }
    }
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let bounded = quantile.clamp(0.0, 1.0);
    let index = ((sorted.len() - 1) as f64 * bounded).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
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
        if metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(KittyError::Invalid(format!(
                "{} has an unsafe owner, hard-link count, or write permission",
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
    let file = options.open(path)?;
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
        if opened.nlink() != 1
            || opened.uid() != unsafe { libc::geteuid() }
            || opened.mode() & 0o077 != 0
        {
            return Err(KittyError::Invalid(format!(
                "{} has an unsafe opened-file owner, hard-link count, or write permission",
                path.display()
            )));
        }
    }
    let mut bytes = Vec::with_capacity(opened.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
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
            if metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(KittyError::Invalid(format!(
                    "{} has an unsafe owner or hard-link count",
                    path.display()
                )));
            }
        }
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".kitty-tmp-{}-{sequence}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
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
        let dir =
            std::env::temp_dir().join(format!("gitrun-kitty-{label}-{}-{now}", std::process::id()));
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
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
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
    fn local_settings_and_dashboard_status_round_trip_without_privileged_paths() {
        let dir = temp_dir("settings-round-trip");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = true;

        save_settings_for_local_state_dir(&dir, &settings).unwrap();
        assert_eq!(load_settings_for_dashboard(&dir).unwrap(), settings);

        let status = load_dashboard_status(&dir).unwrap();
        assert!(status.enabled);
        assert!(status.predictive_warm_pool_enabled);
        assert!(status.shadow_mode);
        assert!(status.repositories.is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saved_settings_flow_through_runtime_into_dashboard_status() {
        let dir = temp_dir("settings-runtime-dashboard-flow");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = false;
        save_settings_for_local_state_dir(&dir, &settings).unwrap();

        let mut runtime = KittyRuntime::new(&dir);
        let decision = runtime.observe(input(1_800, 2, 1), 2, 4, true).unwrap();
        assert_ne!(decision.status, DecisionStatus::SafeFallback);
        runtime.finish_cycle(&decision, 0, true).unwrap();

        let reloaded_settings = load_settings_for_dashboard(&dir).unwrap();
        assert_eq!(reloaded_settings, settings);

        let status = load_dashboard_status(&dir).unwrap();
        assert!(status.enabled);
        assert!(status.telemetry_enabled);
        assert!(status.predictive_analytics_enabled);
        assert!(status.predictive_warm_pool_enabled);
        assert!(!status.shadow_mode);
        assert_eq!(status.repositories.len(), 1);
        assert_eq!(status.repositories[0].repository, "owner/repo");
        assert_eq!(status.repositories[0].sample_count, 1);
        assert_eq!(status.repositories[0].latest_observation_epoch, Some(1_800));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn repository_model_cache_evicts_oldest_without_losing_persisted_history() {
        let dir = temp_dir("model-cache-eviction");
        let settings = enabled_settings();
        write_settings(&dir, &settings);
        let mut runtime = KittyRuntime::new(&dir);
        let repositories = MAX_MODELS + 2;

        for index in 0..repositories {
            let repository = format!("owner/repo-{index}");
            let stamp = 1_800 + (index as u64) * BUCKET_SECONDS;
            let decision = runtime
                .observe(
                    ObservationInput {
                        repository: repository.clone(),
                        observed_at_epoch: stamp,
                        queued_jobs: 0,
                        busy_runners: 1,
                        online_runners: 1,
                        managed_runners: 1,
                        host_cpu_percent: None,
                        host_memory_percent: None,
                        host_disk_percent: None,
                    },
                    2,
                    8,
                    true,
                )
                .unwrap();
            assert_ne!(decision.status, DecisionStatus::SafeFallback);
            runtime.finish_cycle(&decision, 0, true).unwrap();
        }

        assert_eq!(runtime.models.len(), MAX_MODELS);
        assert!(!runtime.models.contains_key(&repository_key("owner/repo-0")));
        let persisted_path = dir
            .join("kitty")
            .join(format!("model-{}.json", repository_key("owner/repo-0")));
        assert!(persisted_path.is_file());

        let status = load_dashboard_status(&dir).unwrap();
        assert!(status.repositories.len() <= MAX_DASHBOARD_REPOSITORIES);
        assert!(status
            .repositories
            .iter()
            .any(|entry| entry.repository == format!("owner/repo-{}", repositories - 1)));

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_model_reader_rejects_symlink_hardlink_and_group_readable_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = temp_dir("private-file-links");
        let original = dir.join("model.json");
        let sym = dir.join("model-symlink.json");
        let hard = dir.join("model-hardlink.json");
        fs::write(&original, br#"{}"#).unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();

        symlink(&original, &sym).unwrap();
        assert!(read_checked(&sym, 1024).is_err());

        fs::hard_link(&original, &hard).unwrap();
        assert!(read_checked(&original, 1024).is_err());
        fs::remove_file(&hard).unwrap();

        fs::set_permissions(&original, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_checked(&original, 1024).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disabling_workflow_model_visibility_hides_repository_forecasts_from_dashboard() {
        let dir = temp_dir("hide-model");
        let mut settings = enabled_settings();
        settings.workflow_model_visible = false;
        write_settings(&dir, &settings);

        let path = KittyRuntime::dashboard_status_path(&dir);
        let status = KittyDashboardStatus {
            schema_version: SETTINGS_SCHEMA_VERSION,
            generated_at_epoch: 20_000,
            enabled: true,
            telemetry_enabled: true,
            predictive_analytics_enabled: true,
            workflow_model_visible: true,
            predictive_warm_pool_enabled: false,
            shadow_mode: true,
            repositories: vec![KittyRepositoryStatus {
                repository: "owner/repo".into(),
                sample_count: 24,
                first_observation_epoch: Some(10_000),
                latest_observation_epoch: Some(20_000),
                latest_forecast: None,
                latest_decision: None,
            }],
        };
        atomic_write_private(&path, &serde_json::to_vec(&status).unwrap()).unwrap();

        let loaded = load_dashboard_status(&dir).unwrap();
        assert!(!loaded.workflow_model_visible);
        assert!(loaded.repositories.is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dashboard_status_reports_off_when_settings_are_missing() {
        let dir = temp_dir("status-default-off");
        let status = load_dashboard_status(&dir).unwrap();
        assert!(!status.enabled);
        assert!(status.shadow_mode);
        assert!(status.repositories.is_empty());
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
        settings.history_buckets = 48;
        settings.warm_pool_min_hold_seconds = 299;
        assert!(settings.validate().is_err());
        settings.warm_pool_min_hold_seconds = 900;
        settings.minimum_quality_score = 0.49;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn reserved_automation_capabilities_cannot_be_enabled() {
        let mut settings = enabled_settings();
        settings.adaptive_resource_management_enabled = true;
        assert!(settings.validate().is_err());
        settings.adaptive_resource_management_enabled = false;
        settings.smart_fast_stop_enabled = true;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn malformed_settings_fail_closed_without_stopping_scheduler() {
        let dir = temp_dir("bad-settings");
        let path = KittyRuntime::settings_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"schema_version":1,"enabled":true,"surprise":true}"#,
        )
        .unwrap();
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
    fn reduced_operator_history_window_is_persisted_after_trimming() {
        let dir = temp_dir("history-window-trim");
        let mut settings = enabled_settings();
        settings.history_buckets = 24;
        settings.minimum_samples = 24;
        write_settings(&dir, &settings);

        let key = repository_key("owner/repo");
        let model_dir = dir.join("kitty");
        fs::create_dir_all(&model_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&model_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let start = 50_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        let mut model = WorkflowModel::empty("owner/repo");
        for index in 0..30u64 {
            model.history.push(Observation {
                bucket_epoch: start + index * BUCKET_SECONDS,
                queued_jobs: 0,
                busy_runners: 1,
                online_runners: 1,
                managed_runners: 2,
                host_cpu_percent: None,
                host_memory_percent: None,
                host_disk_percent: None,
                snapshots_in_bucket: 1,
            });
        }
        model.generated_at_epoch = start + 29 * BUCKET_SECONDS;
        model.decision_audit.push(DecisionAudit {
            bucket_epoch: start + 29 * BUCKET_SECONDS,
            model_version: MODEL_VERSION,
            source_window_start: Some(start),
            source_window_end: Some(start + 29 * BUCKET_SECONDS),
            status: DecisionStatus::RecommendationOnly,
            reason: "sample audit from wider prior history window".into(),
            configured_min_runners: 2,
            proposed_min_runners: 2,
            effective_min_runners: 2,
            predicted_peak_concurrency: Some(1),
            confidence_score: Some(0.7),
            sample_count: 30,
            planned_action_count: Some(0),
            execution_outcome: Some("no_actions_required".into()),
        });
        let path = model_dir.join(format!("model-{key}.json"));
        fs::write(&path, serde_json::to_vec(&model).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let mut runtime = KittyRuntime::new(&dir);
        let current = start + 29 * BUCKET_SECONDS;
        let decision = runtime
            .observe(input(current + 1, 0, 1), 2, 4, true)
            .unwrap();
        runtime.finish_cycle(&decision, 0, true).unwrap();

        let stored: WorkflowModel = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.history.len(), 24);
        assert_eq!(
            stored.history.first().unwrap().bucket_epoch,
            start + 6 * BUCKET_SECONDS
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_bucket_queue_peak_is_persisted_before_the_next_bucket() {
        let dir = temp_dir("same-bucket-peak");
        write_settings(&dir, &enabled_settings());
        let mut runtime = KittyRuntime::new(&dir);

        let first = runtime.observe(input(1_800, 1, 1), 2, 4, true).unwrap();
        runtime.finish_cycle(&first, 0, true).unwrap();

        let burst = runtime.observe(input(1_801, 8, 1), 2, 4, true).unwrap();
        runtime.finish_cycle(&burst, 0, true).unwrap();

        let path = dir
            .join("kitty")
            .join(format!("model-{}.json", repository_key("owner/repo")));
        let stored: WorkflowModel = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stored.history.len(), 1);
        assert_eq!(stored.history[0].queued_jobs, 8);
        assert_eq!(stored.history[0].demand(), 9);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn warm_pool_is_shadow_only_until_explicitly_enabled_and_is_bounded() {
        let dir = temp_dir("shadow");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = true;
        settings.minimum_quality_score = 0.5;
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
        settings.minimum_quality_score = 0.5;
        settings.minimum_samples = 24;
        settings.history_buckets = 48;
        settings.max_additional_warm_runners = 1;
        write_settings(&dir, &settings);

        let mut runtime = KittyRuntime::new(&dir);
        let mut last_decision = None;
        let mut saw_applied_floor = false;
        let start = 10_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        for step in 0..72u64 {
            let stamp = start + step * BUCKET_SECONDS;
            let decision = runtime.observe(input(stamp + 1, 1, 3), 2, 3, true).unwrap();
            if decision.status == DecisionStatus::AppliedWarmPoolFloor {
                saw_applied_floor = true;
                let model_path = dir
                    .join("kitty")
                    .join(format!("model-{}.json", repository_key("owner/repo")));
                assert!(
                    model_path.is_file(),
                    "the model and decision audit must be durable before Kitty returns an applied floor"
                );
                let persisted: WorkflowModel =
                    serde_json::from_slice(&fs::read(&model_path).unwrap()).unwrap();
                assert!(persisted.decision_audit.iter().any(|audit| {
                    audit.status == DecisionStatus::AppliedWarmPoolFloor
                        && audit.effective_min_runners == decision.effective_min_runners
                }));
            }
            runtime.finish_cycle(&decision, 0, true).unwrap();
            last_decision = Some(decision);
        }
        assert!(
            saw_applied_floor,
            "stable, accurate observed demand should eventually qualify for the opt-in floor"
        );
        let decision = last_decision.unwrap();
        assert!(decision.effective_min_runners >= 2);
        assert!(decision.effective_min_runners <= 3);
        assert!(decision.proposed_min_runners <= 3);
        assert!(matches!(
            decision.status,
            DecisionStatus::AppliedWarmPoolFloor
                | DecisionStatus::RecommendationOnly
                | DecisionStatus::InsufficientHistory
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resource_pressure_gate_blocks_predictive_floor_even_with_qualified_history() {
        let dir = temp_dir("pressure-gate");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = false;
        settings.minimum_quality_score = 0.5;
        settings.minimum_samples = 24;
        settings.history_buckets = 48;
        settings.max_additional_warm_runners = 1;
        write_settings(&dir, &settings);

        let mut runtime = KittyRuntime::new(&dir);
        let mut saw_pressure_hold = false;
        let start = 10_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        for step in 0..72u64 {
            let stamp = start + step * BUCKET_SECONDS;
            let decision = runtime
                .observe(input(stamp + 1, 1, 3), 2, 3, false)
                .unwrap();
            assert_ne!(decision.status, DecisionStatus::AppliedWarmPoolFloor);
            assert_eq!(decision.effective_min_runners, 2);
            saw_pressure_hold |= decision.status == DecisionStatus::ResourcePressureHold;
            runtime.finish_cycle(&decision, 0, true).unwrap();
        }
        assert!(saw_pressure_hold);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_forecast_and_prediction_records_fail_model_validation() {
        let settings = enabled_settings();
        let history_limit = 48;
        let mut model = WorkflowModel::empty("owner/repo");
        let start = 50_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        for index in 0..24u64 {
            model.history.push(Observation {
                bucket_epoch: start + index * BUCKET_SECONDS,
                queued_jobs: 1,
                busy_runners: 2,
                online_runners: 3,
                managed_runners: 3,
                host_cpu_percent: None,
                host_memory_percent: None,
                host_disk_percent: None,
                snapshots_in_bucket: 1,
            });
        }
        model.latest_forecast = Some(Forecast {
            model_version: MODEL_VERSION,
            generated_at_epoch: start + 23 * BUCKET_SECONDS,
            target_epoch: start + 23 * BUCKET_SECONDS + 7,
            horizon_seconds: FORECAST_HORIZON_SECONDS,
            sample_count: 24,
            evaluated_forecasts: 0,
            predicted_peak_concurrency: 3,
            mean_absolute_error: None,
            quality_score: 0.8,
            confidence_score: 0.8,
        });
        assert!(model.validate_for("owner/repo", history_limit).is_err());

        model.latest_forecast = None;
        model.predictions.push(PredictionRecord {
            issued_at_bucket: start,
            target_bucket: start + 2 * BUCKET_SECONDS,
            horizon_seconds: 2 * BUCKET_SECONDS,
            predicted_demand: 3,
            actual_demand: None,
            skipped_missing_observation: false,
        });
        assert!(model.validate_for("owner/repo", history_limit).is_err());
    }

    #[test]
    fn snapshot_merging_never_fabricates_combined_peak_demand() {
        let mut sample = Observation {
            bucket_epoch: 1_800,
            queued_jobs: 10,
            busy_runners: 0,
            online_runners: 2,
            managed_runners: 3,
            host_cpu_percent: Some(20.0),
            host_memory_percent: None,
            host_disk_percent: None,
            snapshots_in_bucket: 1,
        };
        let next = Observation {
            bucket_epoch: 1_800,
            queued_jobs: 0,
            busy_runners: 5,
            online_runners: 1,
            managed_runners: 3,
            host_cpu_percent: Some(50.0),
            host_memory_percent: Some(40.0),
            host_disk_percent: None,
            snapshots_in_bucket: 1,
        };
        sample.merge_snapshot(&next);
        assert_eq!(sample.demand(), 10);
        assert_eq!(sample.queued_jobs, 10);
        assert_eq!(sample.busy_runners, 0);
        assert_eq!(sample.snapshots_in_bucket, 2);
    }

    #[test]
    fn settings_reload_restricts_existing_warm_pool_and_shadow_mode_without_restart() {
        let dir = temp_dir("settings-reload-limits");
        let mut settings = enabled_settings();
        settings.predictive_warm_pool_enabled = true;
        settings.shadow_mode = false;
        settings.minimum_quality_score = 0.5;
        settings.warm_pool_min_hold_seconds = 1_800;
        settings.max_additional_warm_runners = 4;
        write_settings(&dir, &settings);

        let mut runtime = KittyRuntime::new(&dir);
        let stamp = 10_000 / BUCKET_SECONDS * BUCKET_SECONDS;
        let first = runtime.observe(input(stamp, 1, 3), 2, 8, true).unwrap();
        runtime.finish_cycle(&first, 0, true).unwrap();

        let key = repository_key("owner/repo");
        {
            let model = runtime
                .models
                .get_mut(&key)
                .expect("the first observation must create a repository model");
            model.decision_audit.push(DecisionAudit {
                bucket_epoch: stamp,
                model_version: MODEL_VERSION,
                source_window_start: Some(stamp.saturating_sub(BUCKET_SECONDS)),
                source_window_end: Some(stamp),
                status: DecisionStatus::AppliedWarmPoolFloor,
                reason: "seed a previously approved floor for the reload regression".into(),
                configured_min_runners: 2,
                proposed_min_runners: 6,
                effective_min_runners: 6,
                predicted_peak_concurrency: Some(6),
                confidence_score: Some(0.95),
                sample_count: 24,
                planned_action_count: Some(1),
                execution_outcome: Some("plan_executed".into()),
            });
        }
        runtime.persist_model(&key).unwrap();

        // Tightening the operator cap while the process is live must clamp an
        // already-approved floor; it must not keep the old, larger value.
        let mut restricted = settings.clone();
        restricted.max_additional_warm_runners = 1;
        write_settings(&dir, &restricted);
        runtime.last_settings_reload =
            Some(Instant::now() - SETTINGS_RELOAD_INTERVAL - Duration::from_secs(1));

        let bounded = runtime
            .observe(input(stamp + BUCKET_SECONDS, 1, 3), 2, 8, true)
            .unwrap();
        assert_eq!(runtime.settings.max_additional_warm_runners, 1);
        assert_eq!(bounded.status, DecisionStatus::WarmPoolHold);
        assert_eq!(bounded.effective_min_runners, 3);
        runtime.finish_cycle(&bounded, 0, true).unwrap();

        // Switching back to shadow mode at runtime must remove the retained
        // active floor, not merely stop raising it on the next forecast.
        let mut shadow = settings.clone();
        shadow.shadow_mode = true;
        write_settings(&dir, &shadow);
        runtime.last_settings_reload =
            Some(Instant::now() - SETTINGS_RELOAD_INTERVAL - Duration::from_secs(1));

        let shadow_decision = runtime
            .observe(input(stamp + 2 * BUCKET_SECONDS, 1, 3), 2, 8, true)
            .unwrap();
        assert!(runtime.settings.shadow_mode);
        assert_eq!(shadow_decision.effective_min_runners, 2);
        assert_ne!(shadow_decision.status, DecisionStatus::WarmPoolHold);
        runtime.finish_cycle(&shadow_decision, 0, true).unwrap();

        // A corrupt replacement must fail closed on hot reload too, while
        // leaving the deterministic scheduler's configured floor untouched.
        let settings_path = KittyRuntime::settings_path(&dir);
        fs::write(
            &settings_path,
            br#"{"schema_version":1,"enabled":true,"surprise":true}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        runtime.last_settings_reload =
            Some(Instant::now() - SETTINGS_RELOAD_INTERVAL - Duration::from_secs(1));

        let corrupt = runtime
            .observe(input(stamp + 3 * BUCKET_SECONDS, 1, 3), 2, 8, true)
            .unwrap();
        assert_eq!(corrupt.status, DecisionStatus::SafeFallback);
        assert_eq!(corrupt.effective_min_runners, 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn warm_pool_hold_is_expired_when_limits_change_or_window_passes() {
        let mut model = WorkflowModel::empty("owner/repo");
        model.decision_audit.push(DecisionAudit {
            bucket_epoch: 10_000,
            model_version: MODEL_VERSION,
            source_window_start: Some(1_000),
            source_window_end: Some(10_000),
            status: DecisionStatus::AppliedWarmPoolFloor,
            reason: "test".into(),
            configured_min_runners: 2,
            proposed_min_runners: 4,
            effective_min_runners: 4,
            predicted_peak_concurrency: Some(4),
            confidence_score: Some(0.9),
            sample_count: 100,
            planned_action_count: Some(1),
            execution_outcome: Some("plan_executed".into()),
        });
        assert_eq!(
            retained_warm_pool_floor(&model, 10_600, 2, 5, 2, 900),
            Some(4)
        );
        assert_eq!(retained_warm_pool_floor(&model, 10_600, 3, 5, 2, 900), None);
        assert_eq!(retained_warm_pool_floor(&model, 11_000, 2, 5, 2, 900), None);
        assert_eq!(
            retained_warm_pool_floor(&model, 10_600, 2, 3, 2, 900),
            Some(3)
        );
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
    fn percentile_preserves_recent_high_demand_without_using_the_absolute_max() {
        let values = [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 4.0, 8.0, 20.0];
        assert_eq!(percentile(&values, 0.0), 0.0);
        assert_eq!(percentile(&values, 0.9), 20.0);
        assert_eq!(percentile(&values, 0.5), 2.0);
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
        let forecast = compute_forecast(&model, &settings, start + 23 * BUCKET_SECONDS).unwrap();
        assert_eq!(forecast.predicted_peak_concurrency, 3);
        assert_eq!(forecast.confidence_score, 0.0);
        assert_eq!(forecast.evaluated_forecasts, 0);
    }

    #[test]
    fn invalid_model_identity_fails_validation() {
        let model = WorkflowModel::empty("another/repo");
        assert!(model.validate_for("owner/repo", 48).is_err());
    }

    #[test]
    fn mismatched_repository_model_fails_closed_without_changing_the_floor() {
        let dir = temp_dir("wrong-model-identity");
        let settings = enabled_settings();
        write_settings(&dir, &settings);

        let model_path = dir
            .join("kitty")
            .join(format!("model-{}.json", repository_key("owner/repo")));
        fs::write(
            &model_path,
            serde_json::to_vec(&WorkflowModel::empty("different/repo")).unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&model_path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let mut runtime = KittyRuntime::new(&dir);
        let decision = runtime.observe(input(1_800, 10, 5), 3, 8, true).unwrap();
        assert_eq!(decision.status, DecisionStatus::SafeFallback);
        assert_eq!(decision.proposed_min_runners, 3);
        assert_eq!(decision.effective_min_runners, 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn settings_symlinks_and_hardlinks_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = temp_dir("unsafe-settings-links");
        let kitty_dir = dir.join("kitty");
        fs::create_dir(&kitty_dir).unwrap();
        fs::set_permissions(&kitty_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let outside = dir.join("settings-target.json");
        fs::write(
            &outside,
            serde_json::to_vec(&KittySettings::default()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();

        let settings_path = KittyRuntime::settings_path(&dir);
        symlink(&outside, &settings_path).unwrap();
        assert!(read_settings(&settings_path).is_err());

        fs::remove_file(&settings_path).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_vec(&KittySettings::default()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o600)).unwrap();
        let hard_link = dir.join("settings-hard-link.json");
        fs::hard_link(&settings_path, &hard_link).unwrap();
        assert!(read_settings(&settings_path).is_err());

        let _ = fs::remove_dir_all(dir);
    }
}
