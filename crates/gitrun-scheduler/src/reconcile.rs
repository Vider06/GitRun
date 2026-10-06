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
    /// Logical workflow jobs that the static validator marked as GitDockRun
    /// targets. A matching queued job gets dedicated dynamic capacity even
    /// when the default runner labels would otherwise satisfy it.
    pub dock_target_jobs: Vec<String>,
    /// Labels configured on every default GitRun runner. Used to distinguish
    /// jobs that can use the warm pool from jobs requiring specialized capacity.
    pub configured_runner_labels: Vec<String>,
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
    },
    /// Runner is online in Docker but GitHub doesn't know about it anymore:
    /// remove and recreate, preserving its permanent/dynamic role.
    RecreateOrphaned { name: String, permanent: bool },
    /// Runner is running but reports offline to GitHub and isn't busy:
    /// restart in place rather than replace.
    RestartUnresponsive { name: String },
    /// Remove an idle container to scale back down toward the minimum.
    RemoveIdle { name: String },
}

/// Determines whether a queued job carries at least one label that is not
/// already present on every default GitRun runner. Such a job needs a
/// dedicated dynamic runner when Logic Containers routes it to specialized
/// capacity.
fn job_requires_specialized_runner(
    job_labels: &[String],
    configured_runner_labels: &[String],
) -> bool {
    job_labels.iter().any(|job_label| {
        let job_label = job_label.trim();
        !job_label.is_empty()
            && !configured_runner_labels
                .iter()
                .map(|label| label.trim())
                .filter(|label| !label.is_empty())
                .any(|label| label.eq_ignore_ascii_case(job_label))
    })
}

/// Computes the desired runner count: enough to cover currently busy runners
/// plus what's queued, clamped to [min, max]. Same formula as the Python
/// original (`min(max, max(min, busy + queued))`).
pub fn desired_count(input: &ReconcileInput) -> u32 {
    let busy = input.runners.iter().filter(|r| r.online && r.busy).count() as u32;
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
    if input.recovery_enabled && input.queued_jobs > 0 {
        for container in &live_containers {
            let runner = by_name(&container.name);
            match runner {
                Some(r) if r.busy => {} // never touch a busy runner
                Some(r) if !r.online => {
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
                    actions.push(Action::RecreateOrphaned {
                        name: container.name.clone(),
                        permanent: container.permanent,
                    });
                }
                _ => {} // online and not busy: healthy, nothing to do
            }
        }
    }

    // 3. Top up to `desired`, permanents first up to `min_runners`, the rest
    // as dynamic overflow. This is the single, non-duplicated version of the
    // Python file's two identical top-up loops.
    let recreating: u32 = actions
        .iter()
        .filter(|a| matches!(a, Action::RecreateOrphaned { .. }))
        .count() as u32;
    let mut current = live_containers.len() as u32 + recreating;
    let mut permanent_count =
        live_containers.iter().filter(|c| c.permanent).count() as u32 + recreating;

    while current < desired && permanent_count < input.min_runners {
        actions.push(Action::CreateRunner {
            permanent: true,
            job_labels: Vec::new(),
            job_name: None,
            job_run_id: None,
        });
        current += 1;
        permanent_count += 1;
    }

    // Specialized queued jobs need capacity in addition to the guaranteed
    // warm pool when the warm pool's default labels don't cover them. This
    // intentionally may raise the total above desired_count, but never
    // above max_runners: the count-based formula assumes runners are
    // interchangeable, which is not true once Logic Containers exist.
    let specialized_jobs: Vec<(Option<String>, Option<u64>, Vec<String>)> = input
        .queued_job_labels
        .iter()
        .enumerate()
        .filter_map(|(index, labels)| {
            let job_name = input.queued_job_names.get(index).cloned();
            let job_run_id = input.queued_job_run_ids.get(index).copied();
            let routed = crate::logic_containers::resolve(&input.logic_rules, labels).is_some();
            let specialized = routed
                && (job_requires_specialized_runner(labels, &input.configured_runner_labels)
                    || job_name
                        .as_deref()
                        .is_some_and(|name| input.dock_target_jobs.iter().any(|target| target == name)));

            specialized.then_some((job_name, job_run_id, labels.clone()))
        })
        .collect();

    let mut specialized_index = 0usize;
    while current < input.max_runners && specialized_index < specialized_jobs.len() {
        let (job_name, job_run_id, job_labels) = &specialized_jobs[specialized_index];
        actions.push(Action::CreateRunner {
            permanent: false,
            job_labels: job_labels.clone(),
            job_name: job_name.clone(),
            job_run_id: *job_run_id,
        });
        specialized_index += 1;
        current += 1;
    }

    // Generic dynamic overflow is used only for queued jobs whose labels are
    // already covered by the global runner label set. Specialized jobs were
    // reserved above, so they aren't duplicated here. There is no per-job
    // identity tracked elsewhere in the system, so this remains positional
    // for the generic overflow path.
    let mut dynamic_index = 0usize;
    while current < desired {
        let next = input
            .queued_job_labels
            .iter()
            .enumerate()
            .filter(|(_, labels)| {
                !(crate::logic_containers::resolve(&input.logic_rules, labels).is_some()
                    && job_requires_specialized_runner(labels, &input.configured_runner_labels))
            })
            .nth(dynamic_index);

        let (job_name, job_run_id, job_labels) = next
            .map(|(index, labels)| {
                (
                    input.queued_job_names.get(index).cloned(),
                    input.queued_job_run_ids.get(index).copied(),
                    labels.clone(),
                )
            })
            .unwrap_or((None, None, Vec::new()));

        actions.push(Action::CreateRunner {
            permanent: false,
            job_labels,
            job_name,
            job_run_id,
        });
        dynamic_index += 1;
        current += 1;
    }

    // 4. Scale down idle containers beyond the minimum, oldest-idle first,
    // preferring to keep permanents (same tie-break as the Python version:
    // sort by (is_permanent, -age) so dynamic+oldest goes first).
    if !input.ephemeral && current > input.min_runners {
        let removable = current - input.min_runners;
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
        for info in candidates.into_iter().take(removable as usize) {
            actions.push(Action::RemoveIdle {
                name: info.name.clone(),
            });
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
            containers: Vec::new(),
            runners: Vec::new(),
            queued_jobs: 0,
            queued_job_labels: Vec::new(),
            queued_job_names: Vec::new(),
            queued_job_run_ids: Vec::new(),
            dock_target_jobs: Vec::new(),
            configured_runner_labels: vec!["self-hosted".into(), "Linux".into()],
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
                    job_labels
                } if job_labels.iter().any(|label| label.eq_ignore_ascii_case("windows"))
            )
        }));
    }

    #[test]
    fn specialized_job_gets_dynamic_capacity_without_consuming_minimum_pool() {
        let mut input = base_input();
        input.queued_jobs = 1;
        input.queued_job_labels = vec![vec!["self-hosted".into(), "windows".into()]];
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
                .filter(|action| {
                    matches!(
                        action,
                        Action::CreateRunner {
                            permanent: true,
                            ..
                        }
                    )
                })
                .count(),
            3
        );
        assert!(actions.contains(&Action::CreateRunner {
            permanent: false,
            job_labels: vec!["self-hosted".into(), "windows".into()],
            job_name: None,
            job_run_id: None,
        }));
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
        input.queued_jobs = 1; // recovery pass only runs when there's demand
        input.runners = Vec::new(); // no matching runner registered anymore
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
    fn recovery_pass_is_skipped_when_no_jobs_queued() {
        // Matches the Python behavior: recovery only runs when `queued > 0`.
        let mut input = base_input();
        input.containers = vec![ContainerView {
            name: "gone-from-github".into(),
            health: ContainerHealth::Running,
            permanent: false,
        }];
        input.queued_jobs = 0;
        input.runners = Vec::new();
        let actions = plan(&input);
        assert!(actions.iter().all(
            |a| !matches!(a, Action::RecreateOrphaned { name, .. } if name == "gone-from-github")
        ));
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
                } => Some(job_labels.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(dynamic.len(), 2);
        assert_eq!(dynamic[0], vec!["gpu".to_string()]);
        assert!(dynamic[1].is_empty());
    }
}
