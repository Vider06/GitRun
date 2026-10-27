# `gitrun-scheduler`

GitRun's runtime control plane. It reconciles GitHub jobs and runner pools, creates/removes/restarts runner containers, performs recovery, applies Logic Containers routing, resolves VM backends and runs GitHub/update services.

Kitty is an optional add-on supplied by `gitrun-smart-scheduler`. A missing settings file disables it; a valid `enabled: true` setting permits model collection and forecasting, while predictive capacity changes require separate opt-in and shadow mode explicitly disabled. High-resolution samples are limited to seven days, while older history is compacted into bounded week/month/year/lifetime summaries; see the [Kitty README](../gitrun-smart-scheduler/README.md). Kitty observes bounded aggregate queue/runner/resource snapshots and may only raise the effective runner floor within operator and global bounds after forecast evaluation. The queue-aware module, introduced in 1.7.0, adds Erlang-C estimates and deterministic offline FCFS replay for evaluation; it is not a live scaling policy. Optional Future Mapping publishes a bounded revisioned one-hour map and Think compares independent windows; neither bypasses GSR, the configured runner maximum, adaptive host-budget checks or resource-pressure backpressure. Kitty errors never abort deterministic reconciliation. PR #83's source-head runtime-contract, reconciliation, VM-resolution and privileged GitDockRun namespace checks are historical 1.5.0 evidence. The complete authorized API disconnect request, live provisioning on real hypervisors and migration of every custom/pre-existing runner image are not covered end to end. PR #103 also added guarded legacy runtime/systemd repair; see the [consolidated release history](../../docs/RELEASE_HISTORY.md).

## Runner network bootstrap

The scheduler can create the configured GitRun-managed runner bridge when Docker's inspect response specifically identifies that network as missing, including Docker's `network gitrun-runner not found` wording. The recovery path does not treat unrelated network names or permission-denied errors as a missing network. Created networks still go through the existing metadata validation and Linux inter-container-isolation configuration; the fix does not relax network policy.

## Host CPU samples

The scheduler's Linux resource sampling supplies host-wide CPU data for resource-pressure decisions and the recent CPU-history view in the dashboard and Management TUI. The UI history is bounded to the latest 60 samples and is not per-job CPU accounting. It is currently process-local and resets when GitRun restarts. This history is distinct from Kitty's bounded training/forecast observations and does not enable Kitty's predictive capacity changes.

