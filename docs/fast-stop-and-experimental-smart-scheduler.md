# Fast Stop and Kitty Smart Scheduler

Updated: 2026-10-20  
Current source line: GitRun 1.5.1 (Fast Stop and Kitty behavior originated in 1.5.0)

Fast Stop and Kitty were introduced in the 1.5.0 source through [PR #83](https://github.com/Vider06/GitRun/pull/83) and remain part of the 1.5.1 source line. Fast Stop is an operator-triggered workflow-run cancellation request. Kitty is an optional statistical model that remains disabled by default and subordinate to deterministic reconciliation. The 1.5.1 legacy-recovery changes are documented separately in [the 1.5.1 release notes](v1.5.1-release-notes.md).

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

Kitty samples aggregate per-repository counts in five-minute buckets: queued self-hosted jobs, busy/online/managed runners and optional host-wide CPU, memory and disk readings. Its one-hour forecast estimates aggregate demand from busy managed runners plus queued jobs. It evaluates forecasts against later observations; missing target data are skipped rather than counted as zero.

Weekday/hour forecasts are evaluated separately from the aggregate forecast. Predictive Warm Pool candidates are workload-specific only when there is recent evidence, at least four active weeks and activity in at least 80% of the observed span. Quality scores are heuristic readiness measures, not calibrated probabilities.

### Retention and data

High-resolution history is capped at 2,016 five-minute buckets (seven days); this is not a seven-day lifetime limit. Older completed samples are compacted into bounded weekly aggregates. Four-week blocks form compact logical months, and twelve such months (48 weeks) form compact logical years, with lifetime summaries retaining long-term workload shape. These fixed-duration tiers are not calendar-month arithmetic. Model count, forecast outcomes, workload predictions, recent occurrences and decision-audit records are also capped to bound memory and JSON size.

Kitty does not store workflow contents, command strings, job payloads, environment values, tokens or secret values. It stores bounded workflow paths, job names, runner labels and GitHub run/job IDs to identify and deduplicate workloads. The recent-job-start sampler is bounded, not an exhaustive ledger; missing starts remain unknown and are not used as fabricated zero outcomes. This limits per-workload error correction until a complete coverage source exists.

### Settings and activation

Settings are stored at:

```text
/var/lib/gitrun/kitty/settings.json
```

A missing file means Kitty is disabled. Store the directory with mode 0700 and the settings/model files with mode 0600, owned by the GitRun service identity. The example settings and full field list are in [the crate README](../crates/gitrun-smart-scheduler/README.md).

By default, Kitty uses shadow mode and does not change runner capacity. Predictive Warm Pool requires the master switch, its own `predictive_warm_pool_enabled` switch, `shadow_mode: false`, fresh/sufficient history and an acceptable forecast-quality score. It may only raise the configured runner floor within both the extra-runner cap and the global maximum. Resource-pressure backpressure still wins, and Kitty cannot lower the operator's configured floor.

Malformed settings, stale data, corrupt/unsafe model state or persistence errors fall back to deterministic scheduling.

### Capabilities not active

Adaptive Resource Management and Smart Fast Stop triggers are reserved settings, not active automation. They remain disabled until GitRun has reliable per-job resource attribution and verified job-level cancellation-outcome evidence. This does not disable the operator-triggered Fast Stop command described above.

## Verification and deployment limits

The PR #83 source-head checks are historical evidence for the 1.5.0 implementation commit, not proof of calibrated real-world prediction accuracy or complete historical job-start coverage. Review Kitty in shadow mode before enabling predictive capacity changes, and check the exact CI results for the source commit being deployed.

The 1.5.1 source includes the guarded legacy recovery path from [PR #103](https://github.com/Vider06/GitRun/pull/103); real desktop privilege/install flows, repair on the target legacy host, provisioning and recovery on real KVM/VirtualBox hosts, a complete authorized disconnect API request, and migration of customized/pre-existing runner images still require environment-specific validation. See [the 1.5.1 release notes](v1.5.1-release-notes.md) and the [historical v1.5.0 status report](v1.5.0-refactor-status.md).
