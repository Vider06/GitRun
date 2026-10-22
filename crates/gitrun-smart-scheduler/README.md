# gitrun-smart-scheduler (Kitty)

For the user-facing feature overview and activation guidance, see [Kitty, Adaptive Resources and Fast Stop](../../docs/SMART_SCHEDULER.md).

Kitty is an optional, deterministic-statistics add-on to GitRun. It is not an LLM or a replacement scheduler. The existing planner remains the authority and default fallback.

## Current capability

- Bounded per-repository observations in five-minute buckets: queued jobs, busy/online/managed runner counts, and optional host-wide CPU/memory/disk percentages.
- Versioned Workflow Model with a seven-day high-resolution observation window plus bounded weekly, compact-month, compact-year and lifetime summaries. Seven days is the raw-sample window, not the total historical retention; the model also provides a one-hour demand forecast, forecast-versus-observed evaluation, a heuristic readiness score and a bounded decision audit.
- Optional Predictive Warm Pool that can only increase the effective runner floor, after explicit opt-in and only within the configured additional-runner cap and global maximum.
- Opt-in Adaptive Resource Management: samples CPU/RAM use for GitHub-authoritative active jobs running in known GitRun-managed containers, then builds per-workload estimates. Three distinct observed job occurrences are required before measured peaks influence an estimate; until then it uses conservative workload-class priors (for example Rust/Cargo, Tauri/GUI, JavaScript and general jobs).
- Adaptive limits are applied only to new local-host runner containers. `GITRUN_CONTAINER_CPUS` and `GITRUN_CONTAINER_MEMORY` remain hard operator ceilings; Kitty accounts for visible cgroup-v2 ancestor limits, subtracts a host reserve, reserves remaining CPU and RAM headroom for already-running managed containers (using cgroup-v2 memory.current so current use isn't counted twice) and shares one CPU/RAM budget across configured repositories in a scheduler poll tick. VM-backed workloads are deferred while adaptive mode is active until a per-VM resource probe is available. If capacity or limits cannot be verified, or the remaining allocation is below the minimum safe floor, creation is deferred. Existing containers are not resized.
- Shadow mode by default for predictive runner-count changes; capacity changes require explicit opt-in, sufficient fresh history and forecast quality at or above the configured threshold.
- Unsafe paths, symlinks, hard links, unexpected owner, corrupt settings/model, excessive file size, missing data, stale observations, and persistence errors fail closed to the deterministic scheduler.

Kitty has no GitHub client, Docker authority, shell execution, API authorization authority, GSR bypass, or Vault access. The scheduler collects GitHub-authoritative active-job metadata and Docker usage, then passes only bounded observations and estimates into Kitty. It does not persist workflow contents, job payloads, secret values, tokens, environment values, or command strings. For recurring-workload identification it stores bounded workflow paths, job names, runner labels, and GitHub run/job IDs for correlation and deduplication. Model files remain private to the scheduler service; the dashboard exposes only summarized resource estimates and bounded recent occurrences.

Adaptive Resource Management is implemented but disabled by default and requires Kitty plus aggregate telemetry. Smart Fast Stop's automatic trigger is also implemented as an optional, GSR-gated workflow-run cancellation; it acts on the entire workflow run, and an accepted GitHub API request is not proof every process has exited. Both need real-host verification before being treated as production-calibrated automation.

## Settings

Kitty is off when its settings file is missing. On Linux, create the private directory and settings file as the same service identity that runs gitrun-autoscaler; use directory mode 0700 and file mode 0600. Default path:

/var/lib/gitrun/kitty/settings.json

Example:

    {
      "schema_version": 1,
      "enabled": true,
      "telemetry_enabled": true,
      "predictive_analytics_enabled": true,
      "predictive_warm_pool_enabled": false,
      "adaptive_resource_management_enabled": false,
      "smart_fast_stop_enabled": false,
      "shadow_mode": true,
      "minimum_samples": 576,
      "history_buckets": 2016,
      "max_additional_warm_runners": 2,
      "minimum_quality_score": 0.85
    }

Changing enabled to true permits Kitty telemetry and forecasts; it does not enable capacity automation. Leave shadow_mode true while comparing predictions with observed demand. To opt into predictive warm-pool changes, separately set predictive_warm_pool_enabled true, explicitly set shadow_mode false, and choose the smallest acceptable max_additional_warm_runners. The existing global maximum and resource-pressure backpressure remain authoritative.

The model files `model-<repository-hash>.json` are stored in the same private directory. Each model retains up to 2,016 five-minute observations (seven days) at high resolution, 256 rolling and season-specific forecast outcome records combined, 256 per-workload prediction records, 256 recent job occurrences, 64 decision-audit records, and bounded summaries at longer horizons. Completed weekly history is compacted into weekly aggregates; each four-week block forms one compact logical month, and twelve compact months (48 weeks) form one compact logical year. Yearly and lifetime summaries preserve longer-term workload shape without retaining every raw bucket. These are fixed-duration compaction tiers, not calendar-month arithmetic, and their collections remain bounded to prevent unbounded model/JSON growth. At most 64 repository models are loaded by a process. No new telemetry or model history is persisted while Kitty is disabled. The current recent-job-start sampler intentionally has bounded run coverage; incomplete history is not treated as proof of zero starts, so workload-level error correction may remain unavailable until complete coverage is wired in.

## Forecast limitations

The aggregate forecast estimates next-hour demand using busy managed runners plus queued self-hosted jobs; it does not forecast per-job duration or queue wait. Resource estimation is separate: it uses high-water CPU/RAM samples for individual active job occurrences and workload-class priors when fewer than three distinct jobs have been observed. Estimates include headroom and remain requests only—the scheduler clamps them to operator and current-host ceilings before Docker applies them. The readiness score is heuristic, not a calibrated probability. Forecast evaluation uses previously issued predictions matched to later observations; missing target buckets are skipped instead of being fabricated as idle.

## Tests

Unit tests cover disabled-by-default behavior, configuration validation, corrupt settings fallback, bucket aggregation, repository path handling, stale/insufficient forecasts, shadow mode and bounded opt-in warm-pool recommendations. These tests passed in the PR #83 source-head CI; rerun the workspace checks after future implementation or integration changes.


## Queue-aware evaluation (1.7.0)

The `queueing` module adds a dependency-free Erlang-C M/M/c estimate, a bounded deterministic FCFS trace replay, and a small runner-floor policy evaluator. Erlang-C is only a baseline: it assumes Poisson arrivals and exponential service times, and reports unstable load explicitly rather than fabricating a finite wait. Replay uses actual service durations from an offline trace and must not leak future durations into historical decision inputs.

These APIs are evaluation primitives in this release, not a replacement for the production deterministic reconciler. No new runner may be created from a model result without the existing maximum-runner, host CPU/RAM budget, resource-pressure, shadow-mode and authorization checks. Compare candidate policies against the existing behavior with replay traces before enabling any future live queue-aware control.


## Future Mapping and Think (1.7.0 source line)

These capabilities are experimental and disabled by default. Future Mapping requires Kitty, aggregate telemetry, predictive analytics, Predictive Warm Pool and Adaptive Resource Management. It builds a private rolling map containing twelve five-minute buckets, predicted concurrency and per-runner CPU/RAM envelopes. It increments a revision on every reconciliation and records typed patches that expire after 60 seconds and can only apply to the immediately preceding revision.

The map's CPU/RAM values are advisory. They are clamped to `GITRUN_CONTAINER_CPUS` and `GITRUN_CONTAINER_MEMORY`; the scheduler's adaptive allocation path still performs host-capacity checks and can defer creation. GSR authorization must match the exact current revision and expiry. Future Mapping itself never creates containers or grants authority.

Think requires Future Mapping and compares 2–5 distinct windows: 1 hour, 6 hours, 24 hours, 3.5 days and 7 days. Each window requires at least 75% of its five-minute sample slots. The default compares 1-hour, 6-hour and 24-hour windows. Missing coverage or disagreement beyond a bounded tolerance blocks new predictive floor growth and workload prewarms; the configured deterministic minimum and normal resource-pressure controls remain in force.

Settings:
- `future_mapping_enabled`: `false` by default; requires Kitty, telemetry, predictive analytics, Predictive Warm Pool and Adaptive Resource Management.
- `think_enabled`: `false` by default; requires Future Mapping.
- `think_iterations`: integer from 2 to 5; defaults to 3.
