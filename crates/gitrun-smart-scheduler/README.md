# gitrun-smart-scheduler (Kitty)

Kitty is an optional, deterministic-statistics add-on to GitRun. It is not an LLM or a replacement scheduler. The existing planner remains the authority and default fallback.

## Current capability

- Bounded per-repository observations in five-minute buckets: queued jobs, busy/online/managed runner counts, and optional host-wide CPU/memory/disk percentages.
- Versioned Workflow Model with a seven-day high-resolution observation window plus bounded weekly, compact-month, compact-year and lifetime summaries. Seven days is the raw-sample window, not the total historical retention; the model also provides a one-hour demand forecast, forecast-versus-observed evaluation, a heuristic readiness score and a bounded decision audit.
- Optional Predictive Warm Pool that can only increase the effective runner floor, after explicit opt-in and only within the configured additional-runner cap and global maximum.
- Shadow mode by default; predictive capacity changes require shadow mode to be explicitly disabled, sufficient fresh history, and forecast quality at or above the configured threshold.
- Unsafe paths, symlinks, hard links, unexpected owner, corrupt settings/model, excessive file size, missing data, stale observations, and persistence errors fail closed to the deterministic runner floor.

Kitty has no GitHub client, Docker access, shell execution, API authorization authority, GSR bypass, or Vault access. It does not persist workflow contents, job payloads, secret values, tokens, environment values, or command strings. For recurring-workload identification it does store bounded workflow paths, job names, runner labels, and GitHub run/job IDs for correlation and deduplication. These are operator-visible metadata, not workflow payloads; model files remain private to the scheduler service, while the dashboard status snapshot exposes bounded recent occurrences and workload profiles. Host-wide resource readings are explicitly not represented as per-job resource attribution.

Adaptive Resource Management and Smart Fast Stop triggers are schema-visible reserved capabilities, not active automation. They must remain disabled until GitRun has reliable per-job resource attribution and active-job duration/cancellation outcome evidence.

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

The first version forecasts the next-hour aggregate demand proxy: busy managed runners plus queued self-hosted jobs. It does not claim to predict per-job runtime, queue wait, or resource requirements; those need reliable timing and workload-attributed measurement data that are not currently wired into the scheduler. The readiness score is heuristic, not a calibrated probability. Forecast evaluation uses previously issued predictions matched to later observations; missing target buckets are skipped instead of being fabricated as idle.

## Tests

Unit tests cover disabled-by-default behavior, configuration validation, corrupt settings fallback, bucket aggregation, repository path handling, stale/insufficient forecasts, shadow mode and bounded opt-in warm-pool recommendations. These tests passed in the PR #83 source-head CI; rerun the workspace checks after future implementation or integration changes.
