//! Lightweight queueing models for Kitty.
//!
//! Erlang-C is an M/M/c baseline, not a guarantee for bursty GitHub Actions
//! workloads. Replay is deterministic and offline-only; it has no I/O or
//! third-party dependencies.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

const MAX_SIMULATED_JOBS: usize = 100_000;
const MAX_RUNNERS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueEstimate {
    pub probability_of_wait: f64,
    pub mean_wait_seconds: f64,
    pub utilization: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueModelError {
    InvalidRate,
    InvalidRunnerCount,
    UnstableQueue,
    TooManyJobs,
    InvalidJobDuration,
}

impl std::fmt::Display for QueueModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidRate => {
                "rates must be finite and non-negative, with positive service rate"
            }
            Self::InvalidRunnerCount => "runner count must be between 1 and 10000",
            Self::UnstableQueue => "arrival rate must be strictly below total service capacity",
            Self::TooManyJobs => "simulation input exceeds the 100000-job safety bound",
            Self::InvalidJobDuration => "job durations must be positive",
        };
        f.write_str(message)
    }
}
impl std::error::Error for QueueModelError {}

/// Estimate M/M/c queue wait with Erlang-C. Unstable queues return an error
/// rather than a misleading finite wait.
pub fn erlang_c(
    arrival_rate_per_second: f64,
    service_rate_per_runner_per_second: f64,
    runners: u32,
) -> Result<QueueEstimate, QueueModelError> {
    if runners == 0 || runners > MAX_RUNNERS {
        return Err(QueueModelError::InvalidRunnerCount);
    }
    if !arrival_rate_per_second.is_finite()
        || !service_rate_per_runner_per_second.is_finite()
        || arrival_rate_per_second < 0.0
        || service_rate_per_runner_per_second <= 0.0
    {
        return Err(QueueModelError::InvalidRate);
    }
    if arrival_rate_per_second == 0.0 {
        return Ok(QueueEstimate {
            probability_of_wait: 0.0,
            mean_wait_seconds: 0.0,
            utilization: 0.0,
        });
    }
    let c = runners as usize;
    let offered_load = arrival_rate_per_second / service_rate_per_runner_per_second;
    let utilization = offered_load / runners as f64;
    if !utilization.is_finite() || utilization >= 1.0 {
        return Err(QueueModelError::UnstableQueue);
    }

    // Iterative Erlang-C sum avoids factorial overflow.
    let mut term = 1.0;
    let mut denominator = 1.0;
    for n in 1..c {
        term *= offered_load / n as f64;
        denominator += term;
    }
    let tail_term = term * offered_load / c as f64;
    let tail = tail_term / (1.0 - utilization);
    denominator += tail;
    let probability_of_wait = (tail / denominator).clamp(0.0, 1.0);
    let spare_capacity =
        runners as f64 * service_rate_per_runner_per_second - arrival_rate_per_second;
    let mean_wait_seconds = probability_of_wait / spare_capacity;
    if !mean_wait_seconds.is_finite() {
        return Err(QueueModelError::UnstableQueue);
    }
    Ok(QueueEstimate {
        probability_of_wait,
        mean_wait_seconds,
        utilization,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueJob {
    pub arrival_seconds: u64,
    /// Actual service duration from a historical trace, not a live prediction.
    pub service_seconds: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayMetrics {
    pub jobs: usize,
    pub mean_wait_seconds: f64,
    pub p95_wait_seconds: u64,
    pub max_wait_seconds: u64,
    pub runner_busy_seconds: u128,
    pub makespan_seconds: u64,
}

/// Deterministic offline FCFS replay for a known arrival/service trace.
/// Historical future durations must never leak into live scaling decisions.
pub fn replay_fcfs(jobs: &[QueueJob], runners: u32) -> Result<ReplayMetrics, QueueModelError> {
    if runners == 0 || runners > MAX_RUNNERS {
        return Err(QueueModelError::InvalidRunnerCount);
    }
    if jobs.len() > MAX_SIMULATED_JOBS {
        return Err(QueueModelError::TooManyJobs);
    }
    if jobs.iter().any(|job| job.service_seconds == 0) {
        return Err(QueueModelError::InvalidJobDuration);
    }
    if jobs.is_empty() {
        return Ok(ReplayMetrics {
            jobs: 0,
            mean_wait_seconds: 0.0,
            p95_wait_seconds: 0,
            max_wait_seconds: 0,
            runner_busy_seconds: 0,
            makespan_seconds: 0,
        });
    }

    let mut ordered = jobs.to_vec();
    ordered.sort_by_key(|job| job.arrival_seconds);
    let mut available: BinaryHeap<Reverse<(u64, u32)>> =
        (0..runners).map(|runner| Reverse((0, runner))).collect();
    let mut waits = Vec::with_capacity(ordered.len());
    let mut runner_busy_seconds = 0_u128;
    let mut makespan_seconds = 0_u64;
    for job in &ordered {
        let Reverse((free_at, runner)) = available.pop().expect("runner heap is non-empty");
        let start = free_at.max(job.arrival_seconds);
        let wait = start.saturating_sub(job.arrival_seconds);
        let finish = start.saturating_add(job.service_seconds);
        waits.push(wait);
        runner_busy_seconds = runner_busy_seconds.saturating_add(job.service_seconds as u128);
        makespan_seconds = makespan_seconds.max(finish);
        available.push(Reverse((finish, runner)));
    }
    waits.sort_unstable();
    let sum_wait = waits.iter().map(|wait| *wait as u128).sum::<u128>();
    let p95_index = (95 * waits.len()).div_ceil(100).saturating_sub(1);
    Ok(ReplayMetrics {
        jobs: waits.len(),
        mean_wait_seconds: sum_wait as f64 / waits.len() as f64,
        p95_wait_seconds: waits[p95_index],
        max_wait_seconds: *waits.last().expect("non-empty input was checked"),
        runner_busy_seconds,
        makespan_seconds,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueAwarePlan {
    pub runner_floor: u32,
    pub estimated_wait_seconds: Option<f64>,
    pub reason: &'static str,
}

/// Return the smallest bounded floor meeting a modeled mean-wait objective.
/// This helper does not authorize runner creation; hard resource/policy gates
/// must be applied by the deterministic scheduler.
pub fn plan_runner_floor(
    current_floor: u32,
    max_runners: u32,
    max_additional_runners: u32,
    arrival_rate_per_second: f64,
    service_rate_per_runner_per_second: f64,
    target_wait_seconds: f64,
) -> Result<QueueAwarePlan, QueueModelError> {
    if current_floor == 0
        || max_runners == 0
        || current_floor > MAX_RUNNERS
        || max_runners > MAX_RUNNERS
    {
        return Err(QueueModelError::InvalidRunnerCount);
    }
    if !target_wait_seconds.is_finite() || target_wait_seconds < 0.0 {
        return Err(QueueModelError::InvalidRate);
    }
    if current_floor >= max_runners {
        let estimate = erlang_c(
            arrival_rate_per_second,
            service_rate_per_runner_per_second,
            current_floor,
        )
        .ok();
        return Ok(QueueAwarePlan {
            runner_floor: current_floor.min(max_runners),
            estimated_wait_seconds: estimate.map(|v| v.mean_wait_seconds),
            reason: "runner floor is already at the configured maximum",
        });
    }
    let upper = current_floor
        .saturating_add(max_additional_runners)
        .min(max_runners);
    for candidate in current_floor..=upper {
        if let Ok(estimate) = erlang_c(
            arrival_rate_per_second,
            service_rate_per_runner_per_second,
            candidate,
        ) {
            if estimate.mean_wait_seconds <= target_wait_seconds {
                return Ok(QueueAwarePlan {
                    runner_floor: candidate,
                    estimated_wait_seconds: Some(estimate.mean_wait_seconds),
                    reason: if candidate == current_floor {
                        "current floor meets the modeled wait objective"
                    } else {
                        "smallest bounded floor meeting the modeled wait objective"
                    },
                });
            }
        }
    }
    let estimate = erlang_c(
        arrival_rate_per_second,
        service_rate_per_runner_per_second,
        upper,
    )
    .ok();
    Ok(QueueAwarePlan {
        runner_floor: current_floor,
        estimated_wait_seconds: estimate.map(|v| v.mean_wait_seconds),
        reason: "wait objective not met within growth bound; retain deterministic floor",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_arrivals_have_zero_wait() {
        let estimate = erlang_c(0.0, 0.1, 2).unwrap();
        assert_eq!(estimate.mean_wait_seconds, 0.0);
        assert_eq!(estimate.probability_of_wait, 0.0);
    }

    #[test]
    fn rejects_unstable_or_invalid_rates() {
        assert_eq!(erlang_c(2.0, 1.0, 2), Err(QueueModelError::UnstableQueue));
        assert_eq!(
            erlang_c(f64::NAN, 1.0, 2),
            Err(QueueModelError::InvalidRate)
        );
        assert_eq!(
            erlang_c(0.1, 1.0, 0),
            Err(QueueModelError::InvalidRunnerCount)
        );
    }

    #[test]
    fn more_runners_reduce_wait() {
        let one = erlang_c(0.08, 0.1, 1).unwrap();
        let two = erlang_c(0.08, 0.1, 2).unwrap();
        assert!(two.mean_wait_seconds < one.mean_wait_seconds);
    }

    #[test]
    fn replay_is_deterministic_and_reports_tail() {
        let jobs = [
            QueueJob {
                arrival_seconds: 0,
                service_seconds: 10,
            },
            QueueJob {
                arrival_seconds: 0,
                service_seconds: 10,
            },
            QueueJob {
                arrival_seconds: 1,
                service_seconds: 4,
            },
        ];
        let result = replay_fcfs(&jobs, 2).unwrap();
        assert_eq!(result.jobs, 3);
        assert_eq!(result.mean_wait_seconds, 3.0);
        assert_eq!(result.p95_wait_seconds, 9);
        assert_eq!(result.max_wait_seconds, 9);
        assert_eq!(result.runner_busy_seconds, 24);
        assert_eq!(result, replay_fcfs(&jobs, 2).unwrap());
    }

    #[test]
    fn planner_respects_growth_bound() {
        let plan = plan_runner_floor(1, 8, 2, 0.09, 0.1, 1.0).unwrap();
        assert!(plan.runner_floor <= 3);
        assert!(plan.runner_floor <= 8);
    }

    #[test]
    fn planner_keeps_floor_when_bound_cannot_meet_target() {
        let plan = plan_runner_floor(2, 8, 1, 0.9, 0.1, 1.0).unwrap();
        assert_eq!(plan.runner_floor, 2);
        assert!(plan.estimated_wait_seconds.is_none());
    }

    #[test]
    fn burst_replay_quantifies_tail_wait_reduction_from_extra_capacity() {
        let jobs: Vec<_> = (0..6)
            .map(|_| QueueJob {
                arrival_seconds: 0,
                service_seconds: 10,
            })
            .collect();
        let one_runner = replay_fcfs(&jobs, 1).unwrap();
        let two_runners = replay_fcfs(&jobs, 2).unwrap();
        assert_eq!(one_runner.p95_wait_seconds, 50);
        assert_eq!(two_runners.p95_wait_seconds, 20);
        assert!(two_runners.mean_wait_seconds < one_runner.mean_wait_seconds);
        assert_eq!(
            one_runner.runner_busy_seconds,
            two_runners.runner_busy_seconds
        );
    }

    #[test]
    fn replay_rejects_zero_duration() {
        assert_eq!(
            replay_fcfs(
                &[QueueJob {
                    arrival_seconds: 0,
                    service_seconds: 0
                }],
                1
            ),
            Err(QueueModelError::InvalidJobDuration)
        );
    }
}
