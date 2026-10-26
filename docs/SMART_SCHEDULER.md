# Kitty Smart Scheduler, Adaptive Resources & Fast Stop

Updated: 2026-10-21  
Working source line: GitRun 1.8.0 source line ( check GitHub Releases for published artifacts; Fast Stop and Kitty originated in 1.5.0)

Fast Stop and Kitty were introduced in the 1.5.0 source through [PR #83](https://github.com/Vider06/GitRun/pull/83) and continue in the 1.8.0 source line. The CLI Fast Stop command is operator-triggered; Smart Fast Stop is a separate optional automatic workflow-run cancellation path with its own policy gates. Kitty is an optional statistical model that remains disabled by default and subordinate to deterministic reconciliation. The legacy-recovery milestone is summarized in the [consolidated release history](RELEASE_HISTORY.md).

## Fast Stop

The CLI command is:

```text
gitrun fast-stop <owner/repo> <workflow-run-id> --yes
```

The dashboard exposes the same operation with a confirmation dialog. The repository must already be configured in GitRun and the run ID must be non-zero. The required `--yes` flag prevents accidental execution from a terminal or script.

GitRun calls GitHub's workflow-run force-cancellation endpoint. **The request cancels the workflow run, not an individual job.** A successful API response means GitHub accepted the cancellation request; it does not confirm that every runner process has exited. GitRun does not kill Docker containers locally as a side effect of this command. The normal runner lifecycle and reconciliation path remain responsible for runner state and cleanup.

API errors and permission failures are returned to the caller. Cancellation latency depends on GitHub and runner behavior; the command does not promise a fixed completion time.

## Kitty Smart Scheduler

Kitty lives in `gitrun-smart-scheduler`. It is a bounded, deterministic-statistics model, not an LLM or an execution engine. It has no GitHub client, Docker authority, shell execution, Vault access or ability to bypass GSR authorization. The existing deterministic reconciler remains authoritative.

### Inputs and forecast

Kitty samples aggregate per-repository counts in five-minute buckets: queued self-hosted jobs, busy/online/managed runners and optional host-wide CPU, memory and disk readings. Its one-hour forecast estimates aggregate demand from busy managed runners plus queued jobs. Separately, optional Adaptive Resource Management polls active GitHub self-hosted jobs on a bounded 30-second cadence, maps the authoritative runner name to a currently running GitRun-managed container, and records Docker CPU/RAM usage against that run/job occurrence. Missing or ambiguous runner mappings and failed stats are skipped rather than guessed. Resource samples are high-water marks per distinct job occurrence and remain in the private model.

Weekday/hour forecasts are evaluated separately from the aggregate forecast. Predictive Warm Pool candidates are workload-specific only when there is recent evidence, at least four active weeks and activity in at least 80% of the observed span. Quality scores are heuristic readiness measures, not calibrated probabilities.

### Retention and data

High-resolution history is capped at 2,016 five-minute buckets (seven days); this is not a seven-day lifetime limit. Older completed samples are compacted into bounded weekly aggregates. Four-week blocks form compact logical months, and twelve such months (48 weeks) form compact logical years, with lifetime summaries retaining long-term workload shape. These fixed-duration tiers are not calendar-month arithmetic. Model count, forecast outcomes, workload predictions, recent occurrences and decision-audit records are also capped to bound memory and JSON size.

Kitty does not store workflow contents, command strings, job payloads, environment values, tokens or secret values. It stores bounded workflow paths, job names, runner labels and GitHub run/job IDs to identify and deduplicate workloads. The recent-job-start sampler is bounded, not an exhaustive ledger; missing starts remain unknown and are not used as fabricated zero outcomes. This limits per-workload error correction until a complete coverage source exists.

### Settings and activation

Settings are stored at:

```
/var/lib/gitrun/kitty/settings.json
```

A missing file means Kitty is disabled. Store the directory with mode 0700 and the settings/model files with mode 0600, owned by the GitRun service identity. The example settings and implementation-specific field details are in the [Kitty crate README](../crates/gitrun-smart-scheduler/README.md).

By default, Kitty is disabled and Predictive Warm Pool is in shadow mode. Predictive Warm Pool requires the master switch, its own `predictive_warm_pool_enabled` switch, `shadow_mode: false`, fresh/sufficient history and an acceptable forecast-quality score. It may only raise the configured runner floor within both the extra-runner cap and the global maximum. Resource-pressure backpressure still wins, and Kitty cannot lower the operator's configured floor.

#### Adaptive Resource Management

This feature is implemented but disabled by default. It requires Kitty and aggregate telemetry to be enabled. Until three distinct jobs of a workload have been observed, Kitty uses a conservative workload-class prior; after that, it uses the 80th percentile of recent per-job CPU/RAM high-water observations plus safety headroom, never below the workload-class prior.

Before creating a local-host Docker runner, the scheduler clamps the request against the hard ceilings `GITRUN_CONTAINER_CPUS` and `GITRUN_CONTAINER_MEMORY`, and against one shared host budget for the entire scheduler poll tick across configured repositories. The probe considers visible cgroup-v2 ancestor limits and current use, reserves remaining CPU and RAM headroom for already-running managed containers, reading cgroup-v2 memory.current so RAM already in use isn't counted twice, and retains at least 20% (and at least half a core) of CPU capacity and 15% (and at least 1 GiB) of RAM for the host, Docker and GitRun. If capacity or existing container limits cannot be verified, or the remaining budget cannot meet the minimum allocation, new runner creation is deferred. VM-backed jobs are deferred in adaptive mode until resource capacity is measured at their own Docker host. Existing containers are never resized. The displayed estimate is before those runtime caps; the applied Docker limits are the actual bounded allocation.

Malformed settings, stale data, corrupt/unsafe model state or persistence errors fall back to deterministic scheduling. If the adaptive feature itself is explicitly enabled but capacity cannot be verified, new runner creation is deferred rather than applying an unbounded guess.

#### Smart Fast Stop

The automatic Smart Fast Stop path is implemented but remains an explicit opt-in. It requires Kitty, Automation mode, a configured repository allowlist, a valid runtime threshold and a fresh GSR authorization. It cancels the entire GitHub workflow run, not an individual job. A successful API response means GitHub accepted the cancellation request; it does not prove every runner process has exited. The separate operator-triggered `gitrun fast-stop <owner/repo> <workflow-run-id> --yes` command remains available independently of the automatic trigger.

## Verification and deployment limits

The PR #83 source-head checks are historical evidence for the 1.5.0 implementation commit, not proof of calibrated real-world prediction accuracy or complete historical job-start coverage. Review Kitty in shadow mode before enabling predictive capacity changes, and check the exact CI results for the source commit being deployed.

The current 1.8.0 source line includes the guarded legacy recovery path from [PR #103](https://github.com/Vider06/GitRun/pull/103) and the protected security-settings read path from [PR #109](https://github.com/Vider06/GitRun/pull/109); real desktop privilege/install flows, repair on the target legacy host, provisioning and recovery on real KVM/VirtualBox hosts, a complete authorized disconnect API request, and migration of customized/pre-existing runner images still require environment-specific validation. See [the 1.6.2 fixes](v1.6.2-fixes.md) and the [consolidated release history](RELEASE_HISTORY.md).


## Queue-aware evaluation introduced in 1.7.0

The `gitrun-smart-scheduler::queueing` module provides three deliberately small primitives:

- **Erlang-C M/M/c estimate:** estimates mean queue wait and probability of waiting from an arrival rate, service rate and runner count. It rejects unstable load (λ ≥ cμ); it is a baseline only and is not a universal upper bound for real CI workloads.
- **Deterministic FCFS replay:** replays a bounded trace of arrival timestamps and actual service durations, reporting mean, P95 and maximum wait, total runner-busy seconds and makespan. This is an offline evaluation tool. A replay benchmark must not use future service durations to make decisions at earlier timestamps.
- **Bounded runner-floor evaluator:** finds the smallest candidate floor meeting a configured modeled-wait target within the explicit growth cap. If the target cannot be met, it retains the deterministic floor. The result is advisory and does not authorize creation of runners.

This release does not silently switch production scaling to MPC or to Erlang-C. Queue estimates are sensitive to their assumptions; bursty arrivals, variable job durations, runner startup delays and host contention must be evaluated with replay/simulation. All live capacity changes remain subject to the deterministic scheduler, hard runner maxima, CPU/RAM budgets, resource-pressure backpressure, explicit opt-in and shadow-mode gates. The module has no external dependencies and is bounded to 100,000 replay jobs and 10,000 modeled runners.


## Future Mapping and Think (1.7.0)

Future Mapping builds a private rolling one-hour projection containing twelve five-minute demand buckets. Each bucket carries predicted concurrency and a per-runner CPU/RAM envelope. When enough distinct workload resource observations exist, the envelope uses a bounded P80 estimate plus headroom; otherwise it uses the configured per-runner ceilings. Neither case replaces the adaptive allocator's shared host budget or proves that resources are currently available.

Every map has a monotonic revision and a one-hour lifetime. A typed patch captures changed fields and has a 60-second expiry; it is accepted only against the immediately preceding revision, with validated bucket indices, timestamps, runner bounds and CPU/RAM ceilings. GSR authorization is accepted for capacity growth only if the exact current revision is still present and actionable at authorization time. A changed, stale or expired map fails closed to the deterministic floor.

Think is an optional guard that compares 2–5 genuinely distinct history windows: 1 hour, 6 hours, 24 hours, 3.5 days and 7 days. Each window requires at least 75% sample coverage. The default uses the first three windows. Incomplete coverage or disagreement beyond a bounded tolerance withholds new predictive floor growth and workload prewarms. Think does not lower the configured minimum, override shadow mode or bypass GSR, resource-pressure controls or operator capacity limits.

Both settings default to disabled and require explicit opt-in. Future Mapping requires Kitty, aggregate telemetry, predictive analytics, Predictive Warm Pool and Adaptive Resource Management. Think requires Future Mapping. Start with shadow-mode observations and review the exact source commit's CI and host-specific behavior; the forecasts are not calibrated service-level guarantees.
