# Fast Stop and Kitty Smart Scheduler

Updated: 2026-10-20  
Version: GitRun 1.5.0

Fast Stop and Kitty are included in the v1.5.0 source merged through [PR #83](https://github.com/Vider06/GitRun/pull/83). Fast Stop is an operator-triggered workflow-run cancellation request. Kitty is an optional statistical model that remains disabled by default and subordinate to deterministic reconciliation.

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

High-resolution history is capped at 2,016 five-minute buckets (seven days). Older history is reduced to bounded weekly, four-week and 48-week/lifetime summaries. These are fixed-duration windows, not calendar-month arithmetic. The implementation also caps loaded repository models, forecast outcomes, workload predictions, occurrences and decision-audit records.

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

The PR #83 source HEAD [`5afbe14`](https://github.com/Vider06/GitRun/commit/5afbe14ef3144b8e27770e2058943eff8614bc7c) passed GitRun CI, CodeQL, DevSkim, Rustfmt and Cargo Lock Maintenance before merge. Those checks cover the committed test suite; they are not evidence of calibrated real-world prediction accuracy or complete historical job-start coverage. Review Kitty in shadow mode before enabling predictive capacity changes.

The separate deployment checks still include the actual desktop privilege/install flows, provisioning and recovery on real KVM/VirtualBox hosts, a complete authorized disconnect API request, and migration of customized or pre-existing runner images. See [the v1.5.0 status report](v1.5.0-refactor-status.md).
