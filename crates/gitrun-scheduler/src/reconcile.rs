//! Pure reconciliation logic: given a snapshot of the current world (Docker
//! containers, GitHub runners, queued jobs) and the desired bounds, decides
//! *what to do* without doing any of it. This is the Rust replacement for the
//! decision-making half of `gitrun_manager.py::reconcile()`.
//!
//! Why pure: the Python version interleaves "decide" and "do" in one function
//! that also happens to call the GitHub API and Docker directly, which makes
//! it untestable without a live daemon and a network connection. Splitting
//! decision from execution means this module has zero I/O and can be unit
//! tested exhaustively with plain data — and it's the natural place to plug
//! in Logic Containers later (deciding *which* image per step is the same
//! shape of problem: given a request, decide what should exist).
//!
//! Fix versus the Python original: the "create up to `desired`" block was
//! duplicated verbatim (once before the recovery pass, once after, both
//! reachable and producing identical results since the first pass already
//! satisfies `desired` in the normal case). Here it's a single step in the
//! plan, computed once from the post-recovery state.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerHealth {
    Running,
    Exited,
    Starting,
}

/// A managed Docker container as far as reconciliation cares.
#[derive(Debug, Clone)]
pub struct ContainerView {
    pub name: String,
    pub health: ContainerHealth,
    pub permanent: bool,
}

/// A GitHub Actions runner registration as far as reconciliation cares.
#[derive(Debug, Clone)]
pub struct RunnerView {
    pub name: String,
    pub online: bool,
    pub busy: bool,
}

/// How long a container has been idle (not busy), if known.
#[derive(Debug, Clone)]
pub struct IdleInfo {
    pub name: String,
    pub idle_for: Duration,
}

#[derive(Debug, Clone)]
pub struct ReconcileInput {
    pub min_runners: u32,
    pub max_runners: u32,
    /// GitRun-managed runner containers reserved by active GitDockRun bindings.
    /// They are excluded from ordinary planning but still consume the repository's global capacity.
    pub reserved_runners: u32,
    pub containers: Vec<ContainerView>,
    pub runners: Vec<RunnerView>,
    pub queued_jobs: u32,
    /// Labels of each queued self-hosted job, one entry per job, in the
    /// same order they should be served. May be shorter than `queued_jobs`
    /// (e.g. label lookup failed for some jobs) or empty entries for jobs
    /// with no extra labels beyond `self-hosted` — `plan()` treats a
    /// missing entry the same as an empty label set, which
    /// `logic_containers::resolve` always falls through to the default for.
    pub queued_job_labels: Vec<Vec<String>>,
    /// GitHub Actions job names aligned with queued_job_labels.
    /// Missing names are treated as unknown and are never used to create a
    /// pre-marked Dock target.
    pub queued_job_names: Vec<String>,
    /// GitHub workflow run IDs aligned with queued_job_names.
    pub queued_job_run_ids: Vec<u64>,
    /// Unique GitHub Actions job IDs aligned with queued_job_names/run IDs.
    pub queued_job_ids: Vec<u64>,
    /// Recurrence-based workload prewarm targets, already authorized by GSR
    /// and filtered against target/period-tagged containers in the live inventory.
    pub workload_prewarms: Vec<gitrun_smart_scheduler::WorkloadPrewarm>,
    /// Logical workflow jobs that the static validator marked as GitDockRun
    /// targets. A matching queued job gets dedicated dynamic capacity even
    /// when the default runner labels would otherwise satisfy it.
    pub dock_target_jobs: Vec<String>,
    /// Logic Containers rules loaded for this reconciliation cycle. A queued
    /// job matching one of these rules requires dedicated dynamic capacity
    /// so it can be created with the selected backend and image.
    pub logic_rules: Vec<crate::logic_containers::LogicRule>,
    pub idle: Vec<IdleInfo>,
    pub recovery_enabled: bool,
    pub recovery_cooldown: Duration,
    /// Per-container age since it last entered the "needs recovery" state,
    /// if it's already being watched. `None` means "not currently tracked".
    pub recovery_age: Vec<(String, Duration)>,
    pub ephemeral: bool,
    pub idle_timeout: Duration,
}

/// A single action the caller should execute. Kept as data (not a closure or
/// trait object) so tests can assert on the exact plan without mocking I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Remove a container that Docker reports as exited, plus deregister it
    /// from GitHub if still registered there.
    RemoveExited { name: String },
    /// Create a new runner container. `permanent` containers count toward
    /// the warm minimum pool; non-permanent are scale-up overflow.
    /// `job_labels` carries the labels of the specific queued job this
    /// runner is being created for, if any — empty for permanent
    /// warm-pool runners (not created "for" any one job) and for dynamic
    /// overflow when the queue had fewer label-tagged jobs than runners
    /// being created (falls back to the default backend/image, same as
    /// before this field existed).
    CreateRunner {
        permanent: bool,
        job_labels: Vec<String>,
        job_name: Option<String>,
        job_run_id: Option<u64>,
        /// Exact queued job occurrence; this labels the runner for audit, but
        /// GitHub still assigns jobs through the workflow's runs-on labels.
        job_id: Option<u64>,
    },
    /// A time-targeted capacity reservation based on a recurring workload
    /// profile. The workload code is metadata, not a GitHub job-pinning API.
    PrewarmWorkload {
        workload_code: String,
        target_epoch: u64,
        hold_until_epoch: u64,
        job_name: String,
        job_labels: Vec<String>,
    },
    /// Runner is online in Docker but GitHub doesn't know about it anymore:
    /// remove and recreate, preserving its permanent/dynamic role.
    RecreateOrphaned { name: String, permanent: bool },
    /// A non-busy managed container is transitional or its runner reports
    /// offline: restart in place rather than replacing its identity.
    RestartUnresponsive { name: String },
    /// Remove an idle container to scale back down toward the minimum.
    RemoveIdle { name: String },
}

/// Returns whether a queued job needs a dedicated dynamic runner.
///
/// GitDockRun targets always need their own runner, even when the default
/// labels already match and no Logic Containers rule is configured. Other
/// jobs need dedicated capacity only when a Logic Containers rule routes
/// them and the default runner labels do not cover the request.
fn job_needs_dedicated_runner(input: &ReconcileInput, index: usize, labels: &[String]) -> bool {
    let is_dock_target = input
        .queued_job_names
        .get(index)
        .is_some_and(|name| input.dock_target_jobs.iter().any(|target| target == name));

    // A matching Logic Containers rule selects a specific backend/image,
    // so it must receive a dynamic runner even if its labels happen to match
    // the default warm pool. GitDockRun targets likewise need isolated
    // per-job capacity regardless of label coverage or routing rules.
    is_dock_target || crate::logic_containers::resolve(&input.logic_rules, labels).is_some()
}

/// Computes the desired runner count: enough to cover currently busy runners
/// plus what's queued, clamped to [min, max]. Same formula as the Python
/// original (`min(max, max(min, busy + queued))`).
pub fn desired_count(input: &ReconcileInput) -> u32 {
    // A runner reported busy must still count as occupied even if GitHub
    // simultaneously reports it offline. Recovery deliberately never
    // restarts a busy runner, so excluding it here would overprovision based
    // on the same inconsistent snapshot.
    let busy = input.runners.iter().filter(|r| r.busy).count() as u32;
    busy.saturating_add(input.queued_jobs)
        .clamp(input.min_runners, input.max_runners)
}

/// Produces the ordered list of actions to converge toward the desired state.
/// Callers execute these in order; each `Action` is independent enough to be
/// retried individually if one step fails, rather than aborting the whole
/// reconcile pass (unlike the Python version, which lets one exception in
/// `reconcile()` skip the entire rest of that repo's cycle).
pub fn plan(input: &ReconcileInput) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut live_containers: Vec<ContainerView> = Vec::new();

    // 1. Drop anything Docker already reports as exited.
    for container in &input.containers {
        if container.health == ContainerHealth::Exited {
            actions.push(Action::RemoveExited {
                name: container.name.clone(),
            });
        } else {
            live_containers.push(container.clone());
        }
    }

    let desired = desired_count(input);
    let by_name = |name: &str| input.runners.iter().find(|r| r.name == name);

    // 2. Recovery pass: reconcile Docker-vs-GitHub disagreement before
    // deciding how many *more* containers to create, so a container that's
    // about to be recreated isn't double-counted as "already satisfying
    // desired".
    if input.recovery_enabled {
        for container in &live_containers {
            let runner = by_name(&container.name);
            match runner {
                Some(r) if r.busy => {} // never touch a busy runner
                Some(r) if !r.online || container.health != ContainerHealth::Running => {
                    let age = input
                        .recovery_age
                        .iter()
                        .find(|(name, _)| name == &container.name)
                        .map(|(_, age)| *age)
                        .unwrap_or(input.recovery_cooldown);
                    if age >= input.recovery_cooldown {
                        actions.push(Action::RestartUnresponsive {
                            name: container.name.clone(),
                        });
                    }
                }
                None => {
                    // A missing registration is ambiguous while a workflow job
                    // may still be running under this runner name. The caller
                    // marks such jobs as busy; otherwise require the same
                    // recovery cooldown used for offline registrations.
                    let age = input
                        .recovery_age
                        .iter()
                        .find(|(name, _)| name == &container.name)
                        .map(|(_, age)| *age)
                        .unwrap_or_default();
                    if age >= input.recovery_cooldown {
                        actions.push(Action::RecreateOrphaned {
                            name: container.name.clone(),
                            permanent: container.permanent,
                        });
                    }
                }
                _ => {} // online and not busy: healthy, nothing to do
            }
        }
    }

    // 3. Top up to `desired`, permanents first up to `min_runners`, the rest
    // as dynamic overflow. This is the single, non-duplicated version of the
    // Python file's two identical top-up loops.
    // Recreating an orphan replaces its existing container; it does not
    // consume an additional slot. Count permanent containers by their actual
    // role rather than treating every orphan as permanent.
    let mut current = (live_containers.len() as u32).saturating_add(input.reserved_runners);
    let mut permanent_count = live_containers.iter().filter(|c| c.permanent).count() as u32;

    // The permanent warm pool is a separate invariant from total demand:
    // dynamic/orphaned runners must not satisfy the configured permanent
    // minimum. Respect the global maximum even when the snapshot is already
    // at or above the desired total.
    while current < input.max_runners && permanent_count < input.min_runners {
        actions.push(Action::CreateRunner {
            permanent: true,
            job_labels: Vec::new(),
            job_name: None,
            job_run_id: None,
            job_id: None,
        });
        current += 1;
        permanent_count += 1;
    }

    // Jobs matched by Logic Containers and GitDockRun targets need capacity
    // in addition to the guaranteed warm pool. This intentionally may raise
    // the total above desired_count, but never above max_runners, including
    // containers reserved by the GitDockRun lifecycle: the
    // count-based formula assumes runners are interchangeable, which is not
    // true when jobs require a particular backend/image or isolated target.
    let specialized_jobs: Vec<(Option<String>, Option<u64>, Option<u64>, Vec<String>)> = input
        .queued_job_labels
        .iter()
        .enumerate()
        .filter_map(|(index, labels)| {
            let job_name = input.queued_job_names.get(index).cloned();
            let job_run_id = input.queued_job_run_ids.get(index).copied();
            let job_id = input.queued_job_ids.get(index).copied();
            let specialized = job_needs_dedicated_runner(input, index, labels);

            specialized.then_some((job_name, job_run_id, job_id, labels.clone()))
        })
        .collect();

    let mut specialized_index = 0usize;
    while current < input.max_runners && specialized_index < specialized_jobs.len() {
        let (job_name, job_run_id, job_id, job_labels) = &specialized_jobs[specialized_index];
        actions.push(Action::CreateRunner {
            permanent: false,
            job_labels: job_labels.clone(),
            job_name: job_name.clone(),
            job_run_id: *job_run_id,
            job_id: *job_id,
        });
        specialized_index += 1;
        current += 1;
    }

    // Generic dynamic overflow is used only for queued jobs whose labels are
    // already covered by the global runner label set. Specialized jobs were
    // reserved above, so they aren't duplicated here. This still pairs jobs
    // with runners in queue order, carrying run/job IDs for audit and GSR
    // binding; GitHub dispatch remains label-based and is not an API-level pin.
    let mut dynamic_index = 0usize;
    while current < desired {
        let next = input
            .queued_job_labels
            .iter()
            .enumerate()
            .filter(|(index, labels)| !job_needs_dedicated_runner(input, *index, labels))
            .nth(dynamic_index);

        let (job_name, job_run_id, job_id, job_labels) = next
            .map(|(index, labels)| {
                (
                    input.queued_job_names.get(index).cloned(),
                    input.queued_job_run_ids.get(index).copied(),
                    input.queued_job_ids.get(index).copied(),
                    labels.clone(),
                )
            })
            .unwrap_or((None, None, None, Vec::new()));

        actions.push(Action::CreateRunner {
            permanent: false,
            job_labels,
            job_name,
            job_run_id,
            job_id,
        });
        dynamic_index += 1;
        current += 1;
    }

    // Recurrence-based warmups are considered only after real queued jobs and
    // Logic Containers targets have consumed capacity. GSR authorization and
    // inventory-based duplicate suppression happen before this pure planner.
    let mut scheduled_prewarm_count = 0u32;
    for workload in &input.workload_prewarms {
        for _ in 0..workload.runner_count {
            if current >= input.max_runners {
                break;
            }
            actions.push(Action::PrewarmWorkload {
                workload_code: workload.workload_code.clone(),
                target_epoch: workload.target_epoch,
                hold_until_epoch: workload.hold_until_epoch,
                job_name: workload.job_name.clone(),
                job_labels: workload.labels.clone(),
            });
            current = current.saturating_add(1);
            scheduled_prewarm_count = scheduled_prewarm_count.saturating_add(1);
        }
        if current >= input.max_runners {
            break;
        }
    }

    // 4. Scale down idle containers beyond the minimum, oldest-idle first,
    // preferring to keep permanents (same tie-break as the Python version:
    // sort by (is_permanent, -age) so dynamic+oldest goes first).
    // Reserved Dock containers count against the global maximum, but they
    // must not reduce the ordinary runner pool's minimum guarantee.
    let minimum_total = input
        .min_runners
        .saturating_add(input.reserved_runners)
        .saturating_add(scheduled_prewarm_count);
    if !input.ephemeral && current > minimum_total {
        let removable = current - minimum_total;
        let mut candidates: Vec<&IdleInfo> = input
            .idle
            .iter()
            .filter(|i| {
                i.idle_for >= input.idle_timeout && live_containers.iter().any(|c| c.name == i.name)
            })
            .collect();
        candidates.sort_by_key(|i| {
            let permanent = live_containers
                .iter()
                .find(|c| c.name == i.name)
                .map(|c| c.permanent)
                .unwrap_or(false);
            (permanent, std::cmp::Reverse(i.idle_for))
        });
        let mut remaining = removable;
        for info in candidates {
            if remaining == 0 {
                break;
            }
            let is_permanent = live_containers
                .iter()
                .find(|container| container.name == info.name)
                .is_some_and(|container| container.permanent);
            // Never scale below the permanent warm-pool guarantee just to
            // reach the total minimum. If dynamic capacity is not idle yet,
            // temporarily keep the extra container instead.
            if is_permanent && permanent_count <= input.min_runners {
                continue;
            }
            actions.push(Action::RemoveIdle {
                name: info.name.clone(),
            });
            remaining -= 1;
            if is_permanent {
                permanent_count = permanent_count.saturating_sub(1);
            }
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> ReconcileInput {
        ReconcileInput {
            min_runners: 3,
            max_runners: 8,
            reserved_runners: 0,
            containers: Vec::new(),
            runners: Vec::new(),
            queued_jobs: 0,
            queued_job_labels: Vec::new(),
            queued_job_names: Vec::new(),
            queued_job_run_ids: Vec::new(),
            queued_job_ids: Vec::new(),
            workload_prewarms: Vec::new(),
            dock_target_jobs: Vec::new(),
            logic_rules: Vec::new(),
            idle: Vec::new(),
            recovery_enabled: true,
            recovery_cooldown: Duration::from_secs(60),
            recovery_age: Vec::new(),
            ephemeral: false,
            idle_timeout: Duration::from_secs(120),
        }
    }

    #[test]
    fn desired_count_respects_minimum_when_idle() {
        let input = base_input();
        assert_eq!(desired_count(&input), 3);
    }

    #[test]
    fn desired_count_saturates_before_clamping() {
        let mut input = base_input();
        input.min_runners = 1;
        input.max_runners = u32::MAX;
        input.queued_jobs = u32::MAX;
        input.runners = vec![RunnerView {
            name: "busy".into(),
            online: true,
            busy: true,
        }];
        assert_eq!(desired_count(&input), u32::MAX);
    }

    #[test]
    fn desired_count_includes_busy_runner_even_if_reported_offline() {
        let mut input = base_input();
        input.min_runners = 1;
        input.max_runners = 8;
        input.runners = vec![RunnerView {
            name: "offline-but-busy".into(),
            online: false,
            busy: true,
        }];
        input.queued_jobs = 1;

        assert_eq!(desired_count(&input), 2);
    }

    #[test]
    fn desired_count_scales_with_queue_up_to_maximum() {
        let mut input = base_input();
        input.queued_jobs = 20;
        assert_eq!(desired_count(&input), 8);
    }

    #[test]
    fn desired_count_counts_busy_runners() {
        let mut input = base_input();
        input.runners = vec![
            RunnerView {
                name: "a".into(),
                online: true,
                busy: true,
            },
            RunnerView {
                name: "b".into(),
                online: true,
                busy: true,
            },
        ];
        input.queued_jobs = 1;
        assert_eq!(desired_count(&input), 3); // 2 busy + 1 queued = 3, still >= min
    }

    #[test]
    fn empty_state_creates_exactly_min_runners_as_permanent() {
        let input = base_input();
        let actions = plan(&input);
        let created: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::CreateRunner { .. }))
            .collect();
        assert_eq!(created.len(), 3);
        assert!(created.iter().all(|a| matches!(
            a,
            Action::CreateRunner {
                permanent: true,
                ..
            }
        )));
    }

    #[test]
    fn unmatched_special_label_does_not_force_specialized_capacity() {
        let mut input = base_input();
        input.queued_jobs = 1;
        input.queued_job_labels = vec![vec!["self-hosted".into(), "windows".into()]];
        input.logic_rules = Vec::new();
        let actions = plan(&input);
        assert!(!actions.iter().any(|action| {
            matches!(
                action,
                Action::CreateRunner {
                    permanent: false,
                    job_labels,
                    ..
                } if job_labels.iter().any(|label| label.eq_ignore_ascii_case("windows"))
            )
        }));
    }

    #[test]
    fn routed_job_gets_dedicated_capacity_even_when_default_labels_match() {
        let mut input = base_input();
        input.queued_jobs = 1;
        input.queued_job_labels = vec![vec!["self-hosted".into(), "Linux".into()]];
        input.logic_rules = vec![crate::logic_containers::LogicRule {
            name: "linux-special-image".into(),
            match_labels: vec!["Linux".into()],
            backend: crate::logic_containers::Backend::LocalLinux,
            image: "gitrun-runner:custom-linux".into(),
        }];
        let actions = plan(&input);
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                ))
                .count(),
            3
        );
        assert!(actions.contains(&Action::CreateRunner {
            permanent: false,
            job_labels: vec!["self-hosted".into(), "Linux".into()],
            job_name: None,
            job_run_id: None,
            job_id: None,
        }));
    }

    #[test]
    fn dock_target_gets_dedicated_capacity_without_logic_container_rule_or_duplicate() {
        let mut input = base_input();
        input.min_runners = 1;
        input.max_runners = 4;
        input.queued_jobs = 3;
        input.queued_job_labels = vec![
            vec!["self-hosted".into(), "Linux".into()],
            vec!["self-hosted".into(), "Linux".into()],
        ];
        input.queued_job_names = vec!["deploy-production".into(), "build-docs".into()];
        input.queued_job_run_ids = vec![42, 43];
        input.queued_job_ids = vec![420, 430];
        input.dock_target_jobs = vec!["deploy-production".into()];
        input.logic_rules = Vec::new();

        let actions = plan(&input);
        assert!(actions.contains(&Action::CreateRunner {
            permanent: false,
            job_labels: vec!["self-hosted".into(), "Linux".into()],
            job_name: Some("deploy-production".into()),
            job_run_id: Some(42),
            job_id: Some(420),
        }));
        assert!(actions.contains(&Action::CreateRunner {
            permanent: false,
            job_labels: vec!["self-hosted".into(), "Linux".into()],
            job_name: Some("build-docs".into()),
            job_run_id: Some(43),
            job_id: Some(430),
        }));
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::CreateRunner { .. }))
                .count(),
            3,
            "one warm-pool runner, one dedicated GitDockRun runner, and one generic dynamic runner"
        );
    }

    #[test]
    fn orphan_recreation_does_not_double_count_orphan_or_promote_dynamic_to_permanent() {
        let mut input = base_input();
        input.min_runners = 3;
        input.max_runners = 8;
        input.queued_jobs = 1;
        input.containers = vec![ContainerView {
            name: "orphan-dynamic".into(),
            health: ContainerHealth::Running,
            permanent: false,
        }];
        input.runners = Vec::new();
        // Orphan recreation is gated by the configured recovery cooldown.
        input.recovery_age = vec![("orphan-dynamic".into(), Duration::from_secs(60))];

        let actions = plan(&input);
        assert!(actions.contains(&Action::RecreateOrphaned {
            name: "orphan-dynamic".into(),
            permanent: false,
        }));
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                ))
                .count(),
            3,
            "the warm pool must still reach min_runners; the orphan replacement is dynamic"
        );
    }

    #[test]
    fn existing_dynamic_capacity_does_not_satisfy_permanent_warm_pool_minimum() {
        let mut input = base_input();
        input.min_runners = 2;
        input.max_runners = 6;
        input.queued_jobs = 0;
        input.containers = (0..2)
            .map(|i| ContainerView {
                name: format!("dynamic-{i}"),
                health: ContainerHealth::Running,
                permanent: false,
            })
            .collect();

        let actions = plan(&input);
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn specialized_capacity_respects_maximum() {
        let mut input = base_input();
        input.queued_jobs = 20;
        input.queued_job_labels = (0..20).map(|_| vec!["windows".into()]).collect();
        input.logic_rules = vec![crate::logic_containers::LogicRule {
            name: "windows".into(),
            match_labels: vec!["windows".into()],
            backend: crate::logic_containers::Backend::LocalLinux,
            image: "gitrun-runner:windows".into(),
        }];
        let actions = plan(&input);
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::CreateRunner { .. }))
                .count(),
            8
        );
    }

    #[test]
    fn overflow_beyond_minimum_is_dynamic() {
        let mut input = base_input();
        input.queued_jobs = 5; // desired = clamp(0+5, 3, 8) = 5
        let actions = plan(&input);
        let permanent = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                )
            })
            .count();
        let dynamic = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::CreateRunner {
                        permanent: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(permanent, 3);
        assert_eq!(dynamic, 2);
    }

    #[test]
    fn no_duplicate_top_up_when_already_at_desired() {
        // Regression test for the Python bug: the create-up-to-desired block
        // appeared twice. Here we start already at desired with no recovery
        // work needed, and must get zero CreateRunner actions, not the
        // "harmless because already satisfied" duplicate-but-inert behavior.
        let mut input = base_input();
        input.containers = vec![
            ContainerView {
                name: "a".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "b".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "c".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
        ];
        input.runners = vec![
            RunnerView {
                name: "a".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "b".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "c".into(),
                online: true,
                busy: false,
            },
        ];
        let actions = plan(&input);
        assert!(actions
            .iter()
            .all(|a| !matches!(a, Action::CreateRunner { .. })));
    }

    #[test]
    fn transitional_container_is_not_treated_as_exited() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "restarting".into(),
            health: ContainerHealth::Starting,
            permanent: true,
        }];
        let actions = plan(&input);
        assert!(!actions.contains(&Action::RemoveExited {
            name: "restarting".into()
        }));
    }

    #[test]
    fn exited_container_is_flagged_for_removal_and_not_counted() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "dead".into(),
            health: ContainerHealth::Exited,
            permanent: true,
        }];
        let actions = plan(&input);
        assert!(actions.contains(&Action::RemoveExited {
            name: "dead".into()
        }));
        // Since the exited container doesn't count as live, we should still
        // top up to the minimum of 3 permanents.
        let created = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(created, 3);
    }

    #[test]
    fn orphaned_runner_is_recreated_with_same_role() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "gone-from-github".into(),
            health: ContainerHealth::Running,
            permanent: false,
        }];
        input.queued_jobs = 1;
        input.runners = Vec::new(); // no matching runner registered anymore
        input.recovery_age = vec![("gone-from-github".into(), Duration::from_secs(60))];
        let actions = plan(&input);
        assert!(actions.contains(&Action::RecreateOrphaned {
            name: "gone-from-github".into(),
            permanent: false,
        }));
    }

    #[test]
    fn busy_runner_is_never_touched_by_recovery() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "busy-one".into(),
            health: ContainerHealth::Running,
            permanent: true,
        }];
        input.runners = vec![RunnerView {
            name: "busy-one".into(),
            online: false,
            busy: true,
        }];
        input.queued_jobs = 1;
        let actions = plan(&input);
        assert!(actions
            .iter()
            .all(|a| !matches!(a, Action::RestartUnresponsive { name } if name == "busy-one")));
        assert!(actions
            .iter()
            .all(|a| !matches!(a, Action::RecreateOrphaned { name, .. } if name == "busy-one")));
    }

    #[test]
    fn transitional_container_with_online_registration_waits_for_cooldown() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "starting".into(),
            health: ContainerHealth::Starting,
            permanent: true,
        }];
        input.runners = vec![RunnerView {
            name: "starting".into(),
            online: true,
            busy: false,
        }];
        input.recovery_age = vec![("starting".into(), Duration::from_secs(59))];
        assert!(!plan(&input).iter().any(
            |action| matches!(action, Action::RestartUnresponsive { name } if name == "starting")
        ));
        input.recovery_age = vec![("starting".into(), Duration::from_secs(60))];
        assert!(plan(&input).iter().any(
            |action| matches!(action, Action::RestartUnresponsive { name } if name == "starting")
        ));
    }

    #[test]
    fn unresponsive_runner_waits_for_cooldown_before_restart() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "flaky".into(),
            health: ContainerHealth::Running,
            permanent: true,
        }];
        input.runners = vec![RunnerView {
            name: "flaky".into(),
            online: false,
            busy: false,
        }];
        input.queued_jobs = 1;
        input.recovery_age = vec![("flaky".into(), Duration::from_secs(10))]; // below cooldown of 60s
        let actions = plan(&input);
        assert!(!actions.contains(&Action::RestartUnresponsive {
            name: "flaky".into()
        }));

        input.recovery_age = vec![("flaky".into(), Duration::from_secs(90))]; // above cooldown
        let actions = plan(&input);
        assert!(actions.contains(&Action::RestartUnresponsive {
            name: "flaky".into()
        }));
    }

    #[test]
    fn idle_scale_down_prefers_removing_dynamic_first() {
        let mut input = base_input();
        input.containers = vec![
            ContainerView {
                name: "perm-1".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "perm-2".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "perm-3".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "dyn-1".into(),
                health: ContainerHealth::Running,
                permanent: false,
            },
        ];
        input.runners = vec![
            RunnerView {
                name: "perm-1".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "perm-2".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "perm-3".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "dyn-1".into(),
                online: true,
                busy: false,
            },
        ];
        input.idle = vec![
            IdleInfo {
                name: "perm-1".into(),
                idle_for: Duration::from_secs(999),
            },
            IdleInfo {
                name: "dyn-1".into(),
                idle_for: Duration::from_secs(150),
            },
        ];
        let actions = plan(&input);
        // Only 1 is removable (4 live - 3 min), and it must be the dynamic
        // one even though the permanent has been idle far longer.
        let removed: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::RemoveIdle { .. }))
            .collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(
            removed[0],
            &Action::RemoveIdle {
                name: "dyn-1".into()
            }
        );
    }

    #[test]
    fn scale_down_preserves_permanent_warm_pool_when_only_permanents_are_idle() {
        let mut input = base_input();
        input.min_runners = 2;
        input.max_runners = 6;
        input.queued_jobs = 0;
        input.containers = vec![
            ContainerView {
                name: "perm-1".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "perm-2".into(),
                health: ContainerHealth::Running,
                permanent: true,
            },
            ContainerView {
                name: "dynamic-busy".into(),
                health: ContainerHealth::Running,
                permanent: false,
            },
        ];
        input.idle = vec![
            IdleInfo {
                name: "perm-1".into(),
                idle_for: Duration::from_secs(999),
            },
            IdleInfo {
                name: "perm-2".into(),
                idle_for: Duration::from_secs(999),
            },
        ];

        let actions = plan(&input);
        assert!(!actions.iter().any(|action| matches!(
            action,
            Action::RemoveIdle { name } if name == "perm-1" || name == "perm-2"
        )));
    }

    #[test]
    fn ephemeral_mode_never_scales_down() {
        let mut input = base_input();
        input.ephemeral = true;
        input.containers = (0..6)
            .map(|i| ContainerView {
                name: format!("c{i}"),
                health: ContainerHealth::Running,
                permanent: i < 3,
            })
            .collect();
        input.runners = input
            .containers
            .iter()
            .map(|c| RunnerView {
                name: c.name.clone(),
                online: true,
                busy: false,
            })
            .collect();
        input.idle = input
            .containers
            .iter()
            .map(|c| IdleInfo {
                name: c.name.clone(),
                idle_for: Duration::from_secs(9999),
            })
            .collect();
        let actions = plan(&input);
        assert!(actions
            .iter()
            .all(|a| !matches!(a, Action::RemoveIdle { .. })));
    }

    #[test]
    fn orphaned_runner_waits_for_recovery_cooldown() {
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "orphaned".into(),
            health: ContainerHealth::Running,
            permanent: true,
        }];
        input.runners.clear();
        input.recovery_age = vec![("orphaned".into(), Duration::from_secs(59))];
        let actions = plan(&input);
        assert!(!actions.iter().any(
            |action| matches!(action, Action::RecreateOrphaned { name, .. } if name == "orphaned")
        ));

        input.recovery_age = vec![("orphaned".into(), Duration::from_secs(60))];
        let actions = plan(&input);
        assert!(actions.iter().any(
            |action| matches!(action, Action::RecreateOrphaned { name, .. } if name == "orphaned")
        ));
    }

    #[test]
    fn recovery_pass_runs_when_no_jobs_are_queued() {
        // Runner health and orphan cleanup are lifecycle invariants, not a function of queue depth.
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "gone-from-github".into(),
            health: ContainerHealth::Running,
            permanent: false,
        }];
        input.queued_jobs = 0;
        input.runners = Vec::new();
        input.recovery_age = vec![("gone-from-github".into(), Duration::from_secs(60))];
        let actions = plan(&input);
        assert!(actions.iter().any(
            |a| matches!(a, Action::RecreateOrphaned { name, .. } if name == "gone-from-github")
        ));
    }

    #[test]
    fn reserved_dock_runners_count_toward_global_maximum() {
        let mut input = base_input();
        input.min_runners = 3;
        input.max_runners = 8;
        input.reserved_runners = 1;
        input.containers = (0..7)
            .map(|i| ContainerView {
                name: format!("runner-{i}"),
                health: ContainerHealth::Running,
                permanent: i < 3,
            })
            .collect();
        input.runners = input
            .containers
            .iter()
            .map(|container| RunnerView {
                name: container.name.clone(),
                online: true,
                busy: false,
            })
            .collect();
        input.queued_jobs = 20;

        let actions = plan(&input);
        assert!(
            actions
                .iter()
                .all(|action| !matches!(action, Action::CreateRunner { .. })),
            "seven ordinary runners plus one reserved Dock already consume max_runners"
        );
    }

    #[test]
    fn reserved_dock_capacity_does_not_satisfy_permanent_warm_pool_minimum() {
        let mut input = base_input();
        input.min_runners = 3;
        input.max_runners = 8;
        input.reserved_runners = 2;
        input.containers = (0..2)
            .map(|i| ContainerView {
                name: format!("dynamic-{i}"),
                health: ContainerHealth::Running,
                permanent: false,
            })
            .collect();

        let actions = plan(&input);
        let permanent_creates = actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::CreateRunner {
                        permanent: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            permanent_creates, 3,
            "reserved Dock containers are not permanent warm-pool runners"
        );
        assert!(
            2 + input.containers.len() + permanent_creates <= input.max_runners as usize,
            "reserved plus existing plus planned runners must respect the global maximum"
        );
    }

    #[test]
    fn recurring_workload_prewarms_respect_global_maximum_and_hold_metadata() {
        let mut input = base_input();
        input.min_runners = 1;
        input.max_runners = 3;
        input.queued_jobs = 0;
        input.workload_prewarms = vec![gitrun_smart_scheduler::WorkloadPrewarm {
            workload_code: "wk-0123456789abcdef".into(),
            workflow_id: 9,
            workflow_path: ".github/workflows/build.yml".into(),
            job_name: "build-linux".into(),
            labels: vec!["self-hosted".into(), "linux".into()],
            target_epoch: 7_200,
            hold_until_epoch: 8_100,
            runner_count: 5,
            observed_distinct_weeks: 6,
            historical_occurrences_per_week: 5.0,
        }];

        let actions = plan(&input);
        let prewarms: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::PrewarmWorkload {
                    workload_code,
                    target_epoch,
                    hold_until_epoch,
                    job_name,
                    job_labels,
                } => Some((
                    workload_code,
                    *target_epoch,
                    *hold_until_epoch,
                    job_name,
                    job_labels,
                )),
                _ => None,
            })
            .collect();

        // One permanent runner plus no more than two warmups fits the
        // configured three-runner maximum even when the forecast asks for five.
        assert_eq!(prewarms.len(), 2);
        assert!(actions.len() <= input.max_runners as usize);
        assert!(prewarms.iter().all(|(code, target, hold, name, labels)| {
            code.as_str() == "wk-0123456789abcdef"
                && *target == 7_200
                && *hold == 8_100
                && name.as_str() == "build-linux"
                && labels.iter().map(String::as_str).eq(["self-hosted", "linux"])
        }));
        assert!(!actions.iter().any(|action| matches!(action, Action::RemoveIdle { .. })));
    }

    #[test]
    fn dynamic_runners_carry_their_triggering_job_labels() {
        // Logic Containers integration: dynamic (non-permanent) overflow
        // runners should carry the labels of the queued job they're being
        // opened for, positionally, so `resolve_backend_and_image` can
        // route them to the right backend/image. Permanent warm-pool
        // runners are never "for" a specific job, so they always get an
        // empty label set even when queued_job_labels is non-empty.
        let mut input = base_input();
        input.queued_jobs = 3; // desired = clamp(0+3, 1, 8) = 3 -> 1 permanent + 2 dynamic slots.
                               // Force one permanent + two dynamic slots by starting under min.
        input.min_runners = 1;
        input.max_runners = 8;
        input.queued_job_labels = vec![vec!["gpu".into()], vec!["arm64".into(), "large".into()]];
        let actions = plan(&input);
        let dynamic: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::CreateRunner {
                    permanent: false,
                    job_labels,
                    ..
                } => Some(job_labels.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(dynamic.len(), 2);
        assert_eq!(dynamic[0], vec!["gpu".to_string()]);
        assert_eq!(dynamic[1], vec!["arm64".to_string(), "large".to_string()]);

        let permanent: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::CreateRunner {
                    permanent: true,
                    job_labels,
                    ..
                } => Some(job_labels.clone()),
                _ => None,
            })
            .collect();
        assert!(permanent.iter().all(|labels| labels.is_empty()));
    }

    #[test]
    fn missing_job_labels_fall_back_to_empty_not_a_panic() {
        // queued_job_labels shorter than the number of dynamic runners
        // created (e.g. label lookup failed for some jobs) must not panic
        // and must fall back to an empty label set for the uncovered ones.
        let mut input = base_input();
        input.min_runners = 1;
        input.queued_jobs = 3;
        input.queued_job_labels = vec![vec!["gpu".into()]]; // only 1 of 3 covered
        let actions = plan(&input);
        let dynamic: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::CreateRunner {
                    permanent: false,
                    job_labels,
                    ..
                } => Some(job_labels.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(dynamic.len(), 2);
        assert_eq!(dynamic[0], vec!["gpu".to_string()]);
        assert!(dynamic[1].is_empty());
    }
}
