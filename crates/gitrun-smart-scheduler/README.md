# gitrun-smart-scheduler (Kitty)

Kitty is an optional, deterministic-statistics add-on to GitRun. It is not an LLM or a replacement scheduler. The existing planner remains the authority and default fallback.

## Current capability

- Bounded per-repository observations in five-minute buckets: queued jobs, busy/online/managed runner counts, and optional host-wide CPU/memory/disk percentages.
- Versioned Workflow Model with a seven-day high-resolution observation window plus bounded weekly, compact-month, compact-year and lifetime summaries. Seven days is the raw-sample window, not the total historical retention; the model also provides a one-hour demand forecast, forecast-versus-observed evaluation, a heuristic readiness score and a bounded decision audit.
- Optional Predictive Warm Pool that can only increase the effective runner floor, after explicit opt-in and only within the configured additional-runner cap and global maximum.
- Opt-in Adaptive Resource Management: samples CPU/RAM use for GitHub-authoritative active jobs running in known GitRun-managed containers, then builds per-workload estimates. Three distinct observed job occurrences are required before measured peaks influence an estimate; until then it uses conservative workload-class priors (for example Rust/Cargo, Tauri/GUI, JavaScript and general jobs).
- Adaptive limits are applied only to new runner containers. `GITRUN_CONTAINER_CPUS` and `GITRUN_CONTAINER_MEMORY` remain hard operator ceilings; Kitty additionally subtracts a host reserve and shares one CPU/RAM budget across all configured repositories in a scheduler poll tick. If capacity cannot be measured, or the remaining allocation is below the minimum safe floor, creation is deferred. Existing containers are not resized.
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
