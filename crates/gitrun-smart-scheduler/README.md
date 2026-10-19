# gitrun-smart-scheduler (Kitty)

Kitty is an optional, deterministic-statistics add-on to GitRun. It is not an LLM or a replacement scheduler. The existing planner remains the authority and default fallback.

## Current capability

- Bounded per-repository observations in five-minute buckets: queued jobs, busy/online/managed runner counts, and optional host-wide CPU/memory/disk percentages.
- Versioned Workflow Model with seven-day maximum history, a one-hour demand forecast, forecast-versus-observed evaluation, a readiness/quality score, and a bounded decision audit.
- Optional Predictive Warm Pool that can only increase the effective runner floor, after explicit opt-in and only within the configured additional-runner cap and global maximum.
- Shadow mode by default; predictive capacity changes require shadow mode to be explicitly disabled, sufficient fresh history, and forecast quality at or above the configured threshold.
- Unsafe paths, symlinks, hard links, unexpected owner, corrupt settings/model, excessive file size, missing data, stale observations, and persistence errors fail closed to the deterministic runner floor.

Kitty has no GitHub client, Docker access, shell execution, API authorization authority, GSR bypass, or Vault access. It does not collect workflow contents, job payloads, secret values, tokens, environment values, commands, or job names. Host-wide resource readings are explicitly not represented as per-job resource attribution.

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

The model files model-<repository-hash>.json are stored in the same private directory. Each model retains at most seven days of five-minute samples, at most 2016 prediction outcomes, and 128 audit decisions. At most 64 repository models are loaded by a process. No telemetry or model is persisted while Kitty is disabled.

## Forecast limitations

The first version forecasts the next-hour aggregate demand proxy: busy managed runners plus queued self-hosted jobs. It does not claim to predict per-job runtime, queue wait, or resource requirements; those need reliable timing and workload-attributed measurement data that are not currently wired into the scheduler. The readiness score is heuristic, not a calibrated probability. Forecast evaluation uses previously issued predictions matched to later observations; missing target buckets are skipped instead of being fabricated as idle.

## Tests

Unit tests cover disabled-by-default behavior, configuration validation, corrupt settings fallback, bucket aggregation, repository path handling, stale/insufficient forecast behavior, shadow mode, and bounded opt-in warm-pool recommendations. Full workspace CI is still required for each integrated source head.
