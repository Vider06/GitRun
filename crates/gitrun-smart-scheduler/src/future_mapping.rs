//! Bounded hour-ahead capacity mapping and independent-window checks for Kitty.
//!
//! This module is advisory only. It cannot create runners or bypass GSR,
//! deterministic reconciliation, operator ceilings or the adaptive host budget.
use super::{valid_repository, KittyError, Observation, Result, WorkloadResourceObservation, BUCKET_SECONDS, MIN_RESOURCE_PROFILE_JOBS};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
const MAP_BUCKETS: usize = 12;
const MAP_TTL: u64 = 3_600;
const PATCH_TTL: u64 = 60;
const MIN_CPU: f64 = 0.25;
const MIN_MEMORY: u64 = 512 * 1024 * 1024;
const MAX_CPU: f64 = 256.0;
const MAX_MEMORY: u64 = 1_u64 << 60;
const MAX_RUNNERS: u32 = 10_000;
const WINDOWS: [usize; 5] = [12, 72, 288, 672, 2_016];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ThinkWindow {
    pub lookback_buckets: usize,
    pub sample_count: usize,
    pub required_samples: usize,
    pub estimated_peak_demand: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ThinkReport {
    pub stable: bool,
    pub iterations: u8,
    pub selected_peak_demand: u32,
    pub disagreement: u32,
    pub windows: Vec<ThinkWindow>,
    pub reason: String,
}

impl ThinkReport {
    pub fn validate(&self) -> Result<()> {
        if !(2..=5).contains(&self.iterations)
            || self.windows.len() != usize::from(self.iterations)
            || self.selected_peak_demand > 100_000
            || self.reason.len() > 512
            || self.reason.chars().any(char::is_control)
        {
            return Err(KittyError::Invalid("Think report exceeds supported bounds".into()));
        }
        for (index, window) in self.windows.iter().enumerate() {
            if window.lookback_buckets != WINDOWS[index]
                || window.sample_count > window.lookback_buckets
                || window.required_samples != window.lookback_buckets.saturating_mul(3).div_ceil(4)
                || window.estimated_peak_demand.is_some_and(|value| value > 100_000)
            {
                return Err(KittyError::Invalid("Think report contains an invalid history window".into()));
            }
        }
        Ok(())
    }
}

/// Compare 1-hour, 6-hour, 24-hour, 3.5-day and 7-day windows. Each requires
/// 75% coverage. Missing data or disagreement prevents predictive growth.
pub fn calculate_think_report(history: &[Observation], now_epoch: u64, iterations: u8) -> Result<ThinkReport> {
    if !(2..=5).contains(&iterations) || now_epoch == 0 {
        return Err(KittyError::Invalid("Think iterations must be 2–5 and the observation time must be valid".into()));
    }
    let mut windows = Vec::with_capacity(usize::from(iterations));
    for lookback_buckets in WINDOWS.iter().take(usize::from(iterations)) {
        let cutoff = now_epoch.saturating_sub((lookback_buckets.saturating_sub(1) as u64).saturating_mul(BUCKET_SECONDS));
        let values = history.iter()
            .filter(|sample| sample.bucket_epoch >= cutoff && sample.bucket_epoch <= now_epoch)
            .map(Observation::demand).collect::<Vec<_>>();
        let required_samples = lookback_buckets.saturating_mul(3).div_ceil(4);
        let estimated_peak_demand = (values.len() >= required_samples).then(|| estimate_peak(&values));
        windows.push(ThinkWindow {
            lookback_buckets: *lookback_buckets,
            sample_count: values.len(),
            required_samples,
            estimated_peak_demand,
        });
    }
    let estimates = windows.iter().filter_map(|window| window.estimated_peak_demand).collect::<Vec<_>>();
    let selected_peak_demand = estimates.iter().copied().max()
        .unwrap_or_else(|| history.last().map(Observation::demand).unwrap_or_default()).min(100_000);
    let minimum = estimates.iter().copied().min().unwrap_or_default();
    let maximum = estimates.iter().copied().max().unwrap_or_default();
    let disagreement = maximum.saturating_sub(minimum);
    let tolerance = 2_u32.max((f64::from(maximum) * 0.25).ceil() as u32);
    let covered = estimates.len() == usize::from(iterations);
    let stable = covered && disagreement <= tolerance;
    let reason = if !covered {
        "Think is holding predictive growth because one or more history windows have less than 75% coverage.".to_owned()
    } else if stable {
        format!("Think found agreement across {} history windows (spread {}, tolerance {}).", iterations, disagreement, tolerance)
    } else {
        format!("Think is holding predictive growth: history windows disagree by {} runners (tolerance {}).", disagreement, tolerance)
    };
    let report = ThinkReport { stable, iterations, selected_peak_demand, disagreement, windows, reason };
    report.validate()?;
    Ok(report)
}

fn estimate_peak(values: &[u32]) -> u32 {
    if values.is_empty() { return 0; }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = (90 * sorted.len()).div_ceil(100).saturating_sub(1);
    let p90 = sorted[index];
    let split = (values.len() / 2).max(1);
    let older_mean = values[..split].iter().map(|value| u64::from(*value)).sum::<u64>() / split as u64;
    let recent = &values[split..];
    let recent_mean = if recent.is_empty() { older_mean } else {
        recent.iter().map(|value| u64::from(*value)).sum::<u64>() / recent.len() as u64
    };
    let trend = recent_mean.saturating_sub(older_mean).min(u64::from(p90 / 2 + 2)) as u32;
    p90.saturating_add(trend).min(100_000)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FutureMapBucket {
    pub start_epoch: u64,
    pub end_epoch: u64,
    pub predicted_concurrency: u32,
    pub cpu_cores_per_runner: f64,
    pub memory_bytes_per_runner: u64,
    pub planned_pool_cpu_cores: f64,
    pub planned_pool_memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum FutureMapPatchOperation {
    SetForecastPeak { peak_demand: u32 },
    UpdateBucket {
        index: usize,
        predicted_concurrency: u32,
        cpu_cores_per_runner: f64,
        memory_bytes_per_runner: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FutureMapPatch {
    pub base_revision: u64,
    pub revision: u64,
    pub generated_at_epoch: u64,
    pub expires_at_epoch: u64,
    pub target_map_generated_at_epoch: u64,
    pub target_map_expires_at_epoch: u64,
    pub target_max_runners: u32,
    pub target_forecast_peak_concurrency: u32,
    pub target_resource_profile_learned: bool,
    pub target_operator_cpu_limit_cores: f64,
    pub target_operator_memory_limit_bytes: u64,
    pub target_think_report: Option<ThinkReport>,
    pub target_actionable: bool,
    pub target_status_reason: String,
    pub source: String,
    pub operations: Vec<FutureMapPatchOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FutureMapping {
    pub schema_version: u32,
    pub repository: String,
    pub revision: u64,
    pub generated_at_epoch: u64,
    pub expires_at_epoch: u64,
    pub max_runners: u32,
    pub forecast_peak_concurrency: u32,
    pub resource_profile_learned: bool,
    pub operator_cpu_limit_cores: f64,
    pub operator_memory_limit_bytes: u64,
    pub buckets: Vec<FutureMapBucket>,
    pub think_report: Option<ThinkReport>,
    pub actionable: bool,
    pub status_reason: String,
    #[serde(default)]
    pub last_patch: Option<FutureMapPatch>,
}

impl FutureMapping {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        repository: &str,
        generated_at_epoch: u64,
        history: &[Observation],
        resource_observations: &[WorkloadResourceObservation],
        forecast_peak: Option<u32>,
        max_runners: u32,
        operator_cpu_limit_cores: f64,
        operator_memory_limit_bytes: u64,
        actionable: bool,
        think_report: Option<ThinkReport>,
        previous: Option<&FutureMapping>,
    ) -> Result<Self> {
        if !valid_repository(repository) || generated_at_epoch == 0 || history.is_empty()
            || max_runners == 0 || max_runners > MAX_RUNNERS
            || !operator_cpu_limit_cores.is_finite()
            || !(MIN_CPU..=MAX_CPU).contains(&operator_cpu_limit_cores)
            || !(MIN_MEMORY..=MAX_MEMORY).contains(&operator_memory_limit_bytes)
        {
            return Err(KittyError::Invalid("Future Mapping input or operator CPU/RAM ceilings are outside supported bounds".into()));
        }
        if let Some(report) = &think_report { report.validate()?; }
        let revision = match previous {
            Some(mapping) => mapping.revision.checked_add(1).ok_or_else(|| KittyError::Invalid("Future Mapping revision counter is exhausted".into()))?,
            None => 1,
        };
        let mut cpus = resource_observations.iter().map(|sample| sample.cpu_cores_peak)
            .filter(|value| value.is_finite() && *value >= MIN_CPU).collect::<Vec<_>>();
        let mut memories = resource_observations.iter().map(|sample| sample.memory_bytes_peak)
            .filter(|value| *value >= MIN_MEMORY).collect::<Vec<_>>();
        cpus.sort_by(f64::total_cmp);
        memories.sort_unstable();
        let learned = cpus.len() >= MIN_RESOURCE_PROFILE_JOBS && memories.len() >= MIN_RESOURCE_PROFILE_JOBS;
        let cpu_per_runner = if learned {
            let index = cpus.len().saturating_mul(80).div_ceil(100).saturating_sub(1);
            (cpus[index] * 1.15).clamp(MIN_CPU, operator_cpu_limit_cores)
        } else { operator_cpu_limit_cores };
        let memory_per_runner = if learned {
            let index = memories.len().saturating_mul(80).div_ceil(100).saturating_sub(1);
            memories[index].saturating_add(memories[index] / 5).clamp(MIN_MEMORY, operator_memory_limit_bytes)
        } else { operator_memory_limit_bytes };
        let baseline = history.last().map(Observation::demand).unwrap_or_default().min(max_runners);
        let mut peak = forecast_peak.unwrap_or(baseline).max(baseline).min(max_runners);
        if let Some(report) = think_report.as_ref().filter(|report| report.stable) {
            peak = peak.max(report.selected_peak_demand.min(max_runners));
        }
        let mut buckets = Vec::with_capacity(MAP_BUCKETS);
        for index in 0..MAP_BUCKETS {
            let step = (index + 1) as u32;
            let predicted = if peak >= baseline {
                baseline.saturating_add((peak - baseline).saturating_mul(step).div_ceil(MAP_BUCKETS as u32))
            } else {
                baseline.saturating_sub((baseline - peak).saturating_mul(step).div_ceil(MAP_BUCKETS as u32))
            }.min(max_runners);
            let start = generated_at_epoch.saturating_add((index as u64).saturating_mul(BUCKET_SECONDS));
            buckets.push(FutureMapBucket {
                start_epoch: start,
                end_epoch: start.saturating_add(BUCKET_SECONDS),
                predicted_concurrency: predicted,
                cpu_cores_per_runner: cpu_per_runner,
                memory_bytes_per_runner: memory_per_runner,
                planned_pool_cpu_cores: cpu_per_runner * f64::from(predicted),
                planned_pool_memory_bytes: memory_per_runner.saturating_mul(u64::from(predicted)),
            });
        }
        let status_reason = if !actionable {
            "Forecast map is observational only; readiness, shadow-mode, Think or resource-pressure gates are not satisfied.".to_owned()
        } else if !learned {
            "Map is GSR-gated, but CPU/RAM values use per-runner operator ceilings until enough distinct resource samples exist.".to_owned()
        } else {
            "Map is GSR-gated; resource plans use bounded P80 observations plus headroom and remain subordinate to host-budget checks.".to_owned()
        };
        let mut mapping = Self {
            schema_version: SCHEMA_VERSION,
            repository: repository.to_owned(),
            revision,
            generated_at_epoch,
            expires_at_epoch: generated_at_epoch.saturating_add(MAP_TTL),
            max_runners,
            forecast_peak_concurrency: peak,
            resource_profile_learned: learned,
            operator_cpu_limit_cores,
            operator_memory_limit_bytes,
            buckets,
            think_report,
            actionable,
            status_reason,
            last_patch: None,
        };
        if let Some(previous) = previous { mapping.last_patch = Some(make_patch(previous, &mapping, generated_at_epoch)); }
        mapping.validate_for(repository, generated_at_epoch)?;
        Ok(mapping)
    }

    pub fn validate_for(&self, repository: &str, model_generated_at_epoch: u64) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION || !valid_repository(repository)
            || self.repository != repository || self.revision == 0 || self.generated_at_epoch == 0
            || self.generated_at_epoch > model_generated_at_epoch
            || self.expires_at_epoch <= self.generated_at_epoch
            || self.expires_at_epoch > self.generated_at_epoch.saturating_add(MAP_TTL)
            || self.max_runners == 0 || self.max_runners > MAX_RUNNERS
            || self.forecast_peak_concurrency > self.max_runners
            || !self.operator_cpu_limit_cores.is_finite()
            || !(MIN_CPU..=MAX_CPU).contains(&self.operator_cpu_limit_cores)
            || !(MIN_MEMORY..=MAX_MEMORY).contains(&self.operator_memory_limit_bytes)
            || self.buckets.len() != MAP_BUCKETS
            || self.status_reason.len() > 512 || self.status_reason.chars().any(char::is_control)
        {
            return Err(KittyError::Invalid("Future Mapping is malformed or outside hard bounds".into()));
        }
        for (index, bucket) in self.buckets.iter().enumerate() {
            let start = self.generated_at_epoch.saturating_add((index as u64).saturating_mul(BUCKET_SECONDS));
            if bucket.start_epoch != start || bucket.end_epoch != start.saturating_add(BUCKET_SECONDS)
                || bucket.predicted_concurrency > self.max_runners
                || !bucket.cpu_cores_per_runner.is_finite()
                || bucket.cpu_cores_per_runner < MIN_CPU || bucket.cpu_cores_per_runner > self.operator_cpu_limit_cores
                || !(MIN_MEMORY..=self.operator_memory_limit_bytes).contains(&bucket.memory_bytes_per_runner)
                || !bucket.planned_pool_cpu_cores.is_finite() || bucket.planned_pool_cpu_cores < 0.0
                || bucket.planned_pool_cpu_cores > self.operator_cpu_limit_cores * f64::from(self.max_runners)
                || bucket.planned_pool_memory_bytes > self.operator_memory_limit_bytes.saturating_mul(u64::from(self.max_runners))
                || (bucket.planned_pool_cpu_cores - bucket.cpu_cores_per_runner * f64::from(bucket.predicted_concurrency)).abs() > 0.001
                || bucket.planned_pool_memory_bytes != bucket.memory_bytes_per_runner.saturating_mul(u64::from(bucket.predicted_concurrency))
            {
                return Err(KittyError::Invalid("Future Mapping bucket violates its CPU/RAM or time bounds".into()));
            }
        }
        if let Some(report) = &self.think_report { report.validate()?; }
        if self.actionable && self.think_report.as_ref().is_some_and(|report| !report.stable) {
            return Err(KittyError::Invalid("an actionable Future Mapping cannot contain an unstable Think report".into()));
        }
        if let Some(patch) = &self.last_patch {
            if patch.revision != self.revision || patch.base_revision.checked_add(1) != Some(patch.revision)
                || patch.target_map_generated_at_epoch != self.generated_at_epoch
                || patch.target_map_expires_at_epoch != self.expires_at_epoch
                || patch.target_max_runners != self.max_runners
                || patch.target_forecast_peak_concurrency != self.forecast_peak_concurrency
                || patch.target_resource_profile_learned != self.resource_profile_learned
                || patch.target_operator_cpu_limit_cores != self.operator_cpu_limit_cores
                || patch.target_operator_memory_limit_bytes != self.operator_memory_limit_bytes
                || patch.target_think_report != self.think_report
                || patch.target_actionable != self.actionable || patch.target_status_reason != self.status_reason
                || patch.generated_at_epoch == 0 || patch.expires_at_epoch <= patch.generated_at_epoch
                || patch.expires_at_epoch > patch.generated_at_epoch.saturating_add(PATCH_TTL)
                || !matches!(patch.source.as_str(), "queue-reconciliation" | "think-reconciliation" | "resource-reconciliation")
                || patch.operations.is_empty() || patch.operations.len() > MAP_BUCKETS + 1
            {
                return Err(KittyError::Invalid("Future Mapping patch metadata is malformed".into()));
            }
        }
        Ok(())
    }

    pub fn is_actionable_at(&self, now_epoch: u64) -> bool {
        self.actionable && now_epoch >= self.generated_at_epoch && now_epoch < self.expires_at_epoch
    }

    /// Apply a typed patch only to the immediately preceding revision.
    pub fn apply_patch(&mut self, patch: &FutureMapPatch, now_epoch: u64) -> Result<()> {
        if patch.base_revision != self.revision || patch.revision != self.revision.checked_add(1).unwrap_or(0)
            || patch.generated_at_epoch == 0 || patch.generated_at_epoch > now_epoch
            || now_epoch > patch.expires_at_epoch || patch.expires_at_epoch <= patch.generated_at_epoch
            || patch.expires_at_epoch > patch.generated_at_epoch.saturating_add(PATCH_TTL)
            || patch.target_map_generated_at_epoch != patch.generated_at_epoch
            || patch.target_map_expires_at_epoch != patch.target_map_generated_at_epoch.saturating_add(MAP_TTL)
            || patch.target_max_runners == 0 || patch.target_max_runners > MAX_RUNNERS
            || patch.target_forecast_peak_concurrency > patch.target_max_runners
            || !patch.target_operator_cpu_limit_cores.is_finite()
            || !(MIN_CPU..=MAX_CPU).contains(&patch.target_operator_cpu_limit_cores)
            || !(MIN_MEMORY..=MAX_MEMORY).contains(&patch.target_operator_memory_limit_bytes)
            || patch.target_status_reason.len() > 512 || patch.target_status_reason.chars().any(char::is_control)
            || patch.operations.is_empty() || patch.operations.len() > MAP_BUCKETS + 1
            || !matches!(patch.source.as_str(), "queue-reconciliation" | "think-reconciliation" | "resource-reconciliation")
        {
            return Err(KittyError::Invalid("Future Mapping patch is stale or revision-mismatched".into()));
        }
        let mut next = self.clone();
        next.revision = patch.revision;
        next.generated_at_epoch = patch.target_map_generated_at_epoch;
        next.expires_at_epoch = patch.target_map_expires_at_epoch;
        next.max_runners = patch.target_max_runners;
        next.forecast_peak_concurrency = patch.target_forecast_peak_concurrency;
        next.resource_profile_learned = patch.target_resource_profile_learned;
        next.operator_cpu_limit_cores = patch.target_operator_cpu_limit_cores;
        next.operator_memory_limit_bytes = patch.target_operator_memory_limit_bytes;
        next.think_report = patch.target_think_report.clone();
        next.actionable = patch.target_actionable;
        next.status_reason = patch.target_status_reason.clone();
        for operation in &patch.operations {
            match operation {
                FutureMapPatchOperation::SetForecastPeak { peak_demand } => {
                    if *peak_demand > next.max_runners { return Err(KittyError::Invalid("Future Mapping patch exceeds max_runners".into())); }
                    next.forecast_peak_concurrency = *peak_demand;
                }
                FutureMapPatchOperation::UpdateBucket { index, predicted_concurrency, cpu_cores_per_runner, memory_bytes_per_runner } => {
                    let Some(bucket) = next.buckets.get_mut(*index) else { return Err(KittyError::Invalid("Future Mapping patch bucket index is invalid".into())); };
                    if *predicted_concurrency > next.max_runners || !cpu_cores_per_runner.is_finite()
                        || *cpu_cores_per_runner < MIN_CPU || *cpu_cores_per_runner > next.operator_cpu_limit_cores
                        || !(MIN_MEMORY..=next.operator_memory_limit_bytes).contains(memory_bytes_per_runner)
                    { return Err(KittyError::Invalid("Future Mapping patch resource bounds are invalid".into())); }
                    bucket.predicted_concurrency = *predicted_concurrency;
                    bucket.cpu_cores_per_runner = *cpu_cores_per_runner;
                    bucket.memory_bytes_per_runner = *memory_bytes_per_runner;
                }
            }
        }
        for (index, bucket) in next.buckets.iter_mut().enumerate() {
            bucket.start_epoch = next.generated_at_epoch.saturating_add((index as u64).saturating_mul(BUCKET_SECONDS));
            bucket.end_epoch = bucket.start_epoch.saturating_add(BUCKET_SECONDS);
            bucket.planned_pool_cpu_cores = bucket.cpu_cores_per_runner * f64::from(bucket.predicted_concurrency);
            bucket.planned_pool_memory_bytes = bucket.memory_bytes_per_runner.saturating_mul(u64::from(bucket.predicted_concurrency));
        }
        next.last_patch = Some(patch.clone());
        next.validate_for(&next.repository, next.generated_at_epoch)?;
        *self = next;
        Ok(())
    }
}

fn make_patch(old: &FutureMapping, next: &FutureMapping, now: u64) -> FutureMapPatch {
    let mut operations = Vec::new();
    if old.forecast_peak_concurrency != next.forecast_peak_concurrency {
        operations.push(FutureMapPatchOperation::SetForecastPeak { peak_demand: next.forecast_peak_concurrency });
    }
    for (index,(before,after)) in old.buckets.iter().zip(&next.buckets).enumerate() {
        if before.predicted_concurrency != after.predicted_concurrency
            || before.cpu_cores_per_runner != after.cpu_cores_per_runner
            || before.memory_bytes_per_runner != after.memory_bytes_per_runner {
            operations.push(FutureMapPatchOperation::UpdateBucket {
                index, predicted_concurrency: after.predicted_concurrency,
                cpu_cores_per_runner: after.cpu_cores_per_runner,
                memory_bytes_per_runner: after.memory_bytes_per_runner,
            });
        }
    }
    if operations.is_empty() {
        if let Some(first)=next.buckets.first() {
            operations.push(FutureMapPatchOperation::UpdateBucket {
                index:0, predicted_concurrency:first.predicted_concurrency,
                cpu_cores_per_runner:first.cpu_cores_per_runner, memory_bytes_per_runner:first.memory_bytes_per_runner,
            });
        }
    }
    let source = if old.think_report != next.think_report { "think-reconciliation" }
        else if old.resource_profile_learned != next.resource_profile_learned
            || old.buckets.first().map(|b|(b.cpu_cores_per_runner,b.memory_bytes_per_runner))
                != next.buckets.first().map(|b|(b.cpu_cores_per_runner,b.memory_bytes_per_runner)) {
            "resource-reconciliation"
        } else { "queue-reconciliation" };
    FutureMapPatch {
        base_revision: old.revision, revision: next.revision, generated_at_epoch: now,
        expires_at_epoch: now.saturating_add(PATCH_TTL),
        target_map_generated_at_epoch: next.generated_at_epoch, target_map_expires_at_epoch: next.expires_at_epoch,
        target_max_runners: next.max_runners, target_forecast_peak_concurrency: next.forecast_peak_concurrency,
        target_resource_profile_learned: next.resource_profile_learned,
        target_operator_cpu_limit_cores: next.operator_cpu_limit_cores,
        target_operator_memory_limit_bytes: next.operator_memory_limit_bytes,
        target_think_report: next.think_report.clone(), target_actionable: next.actionable,
        target_status_reason: next.status_reason.clone(), source: source.into(), operations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn history(count: usize, demand: u32, now: u64) -> Vec<Observation> {
        (0..count).map(|i| Observation {
            bucket_epoch: now.saturating_sub(((count-i-1) as u64).saturating_mul(BUCKET_SECONDS)),
            queued_jobs: demand.saturating_sub(1), busy_runners: u32::from(demand>0),
            online_runners:2, managed_runners:3, host_cpu_percent:Some(15.0),
            host_memory_percent:Some(40.0), host_disk_percent:Some(20.0), snapshots_in_bucket:1,
        }).collect()
    }

    #[test]
    fn think_compares_distinct_windows_and_holds_on_missing_data() {
        let now=1_800_000_000/BUCKET_SECONDS*BUCKET_SECONDS;
        let full=history(576,3,now);
        let report=calculate_think_report(&full,now,3).unwrap();
        assert!(report.stable);
        assert_eq!(report.windows.len(),3);
        assert_ne!(report.windows[0].lookback_buckets,report.windows[1].lookback_buckets);
        let thin=calculate_think_report(&history(24,3,now),now,3).unwrap();
        assert!(!thin.stable);
        assert!(thin.windows.iter().any(|window|window.estimated_peak_demand.is_none()));
    }

    #[test]
    fn future_mapping_clamps_resources_and_rejects_stale_patch() {
        let now=1_800_000_000/BUCKET_SECONDS*BUCKET_SECONDS;
        let hist=history(576,3,now);
        let report=calculate_think_report(&hist,now,3).unwrap();
        let first=FutureMapping::build("owner/repo",now,&hist,&[],Some(5),8,2.0,
            4*1024*1024*1024,true,Some(report.clone()),None).unwrap();
        assert_eq!(first.buckets.len(),12);
        assert!(first.buckets.iter().all(|b|b.cpu_cores_per_runner<=2.0));
        assert!(first.buckets.iter().all(|b|b.memory_bytes_per_runner<=4*1024*1024*1024));
        assert!(first.is_actionable_at(now));
        let next_time=now+BUCKET_SECONDS;
        let next=FutureMapping::build("owner/repo",next_time,&hist,&[],Some(7),8,2.0,
            4*1024*1024*1024,true,Some(report),Some(&first)).unwrap();
        let patch=next.last_patch.clone().unwrap();
        let mut applied=first.clone();
        applied.apply_patch(&patch,next_time+1).unwrap();
        assert_eq!(applied.revision,next.revision);
        assert_eq!(applied.generated_at_epoch,next.generated_at_epoch);
        assert_eq!(applied.forecast_peak_concurrency,next.forecast_peak_concurrency);
        let mut stale=first;
        assert!(stale.apply_patch(&patch,patch.expires_at_epoch+1).is_err());
    }
}