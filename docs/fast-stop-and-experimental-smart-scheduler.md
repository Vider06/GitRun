# Fast Stop and Experimental Smart Scheduler

**Status:** Fast Stop implementation and tests are included in PR #83; Experimental Smart Scheduler remains a design proposal  
**Target:** Fast Stop is reviewed as part of v1.5.0; Smart Scheduler release placement remains TBD  
**Scope:** Workflow-run force cancellation, workflow telemetry, predictive workflow models, and bounded adaptive scheduling  
**Verification:** PR #83 CI is a gate for the committed source and test suite. Passing CI does not by itself validate every live cancellation race or qualify the Smart Scheduler proposal for release.

## Goals

This proposal adds two related but independently deployable capabilities:

1. **Fast Stop:** make a cancellation request trigger immediate cancellation handling, rather than waiting for a later polling/reconciliation cycle or for the runner to notice cancellation late.
2. **Experimental Smart Scheduler:** learn from actual workflow execution history and use transparent predictions to prepare and size runner capacity more intelligently.

The existing deterministic scheduler remains the baseline and safe fallback. These features must not weaken runner isolation, authorization, Docker/GitDockRun protections, configured capacity limits, or existing policy enforcement.

## 1. Fast Stop

### Observed problem

When a job is cancelled, the job can continue for a noticeable period before the self-hosted runner reacts. The observed runner log sequence appears to show a delay before the runner evaluates running/cancellation conditions and stops the work.

The exact source of the delay must be verified with timestamped logs. Possible contributors include GitHub's cancellation propagation, the runner's processing of the cancellation message, step/job condition evaluation, a GitRun polling interval, a process that does not handle termination, or a container cleanup path. Do not assume that shortening GitRun's polling interval alone will solve a delay that occurs inside the GitHub Actions runner.

### Implemented path in PR #83

- The dashboard lists active workflow runs and exposes a **Fast Stop** action with an explicit confirmation dialog.
- The CLI exposes `gitrun fast-stop <owner/repo> <run_id> --yes`; `--yes` is required so the destructive action is never triggered accidentally in a script or terminal.
- Both entry points accept only repositories already present in GitRun's configured repository allowlist and reject a zero run ID.
- The scheduler's GitHub client uses GitHub's supported `POST /repos/{owner}/{repo}/actions/runs/{run_id}/force-cancel` endpoint. This is designed for runs that remain stuck in cancellation, including workflows whose conditions delay ordinary cancellation.
- **Important scope boundary:** GitHub exposes this force-cancel API at *workflow-run* scope, not individual-job scope. Fast Stop therefore cancels every job in the selected run. The dashboard says this before the user confirms; it must not be described as a single-job kill.
- A successful API response means GitHub accepted the force-cancel request; it does **not** prove every runner process has exited. GitRun does not directly kill Docker containers in this first implementation, avoiding unsafe guesses about job-to-container identity and permanent-runner lifecycle. The UI refreshes the snapshot and surfaces API failures.

### Remaining behavior and safety invariants

- Cancellation begins directly from the user action and does not wait for the ordinary scheduler polling interval.
- If the GitHub API is unavailable or permissions are insufficient, the request fails visibly; GitRun must not claim that cancellation succeeded.
- Do not claim a fixed cancellation latency independent of GitHub's own propagation and runner behavior. Capture timestamps and measure p50/p95/max before promising latency improvements.
- Preserve cleanup, event/log delivery, and state reconciliation. A cancelled job must not leave a runner incorrectly marked busy or permanently block capacity.
- Never remove or restart a permanent runner container merely because a workflow was cancelled. Local process/container termination would require a separate, verified job-to-container binding and lifecycle protocol.
- Repeated force-cancel requests should remain safe; races with run completion, GTUU updates, and scheduler reconciliation must be covered by tests.

### Investigation and implementation plan

1. Trace the complete path from the UI/API cancellation request through GitRun, GitHub's job/workflow cancellation endpoint, runner state updates, and container process termination.
2. Capture timestamps for request received, GitHub cancellation submitted/acknowledged, runner cancellation observed, process termination requested, process exit, and final state update.
3. Reproduce the delay and identify which interval dominates. Include a running step, a step with a long-running child process, a job completing during cancellation, and an unavailable GitHub API.
4. Select the fastest supported runner-notification mechanism. Prefer supported GitHub/runner behavior; use a local process/container signal only when the exact job-to-process mapping is reliable.
5. Add tests for authorization, isolation, duplicate requests, graceful termination, forced termination, partial failures, and races.
6. Report cancellation latency using p50/p95 and maximum observed values, with the measurement boundaries stated explicitly.

### Safety invariants

- Cancellation applies only to the targeted workflow/job and its owned execution context.
- A request being accepted is not proof that execution has stopped.
- If the GitHub API is unavailable, local termination behavior must follow an explicit policy and must not falsely report remote cancellation success.
- Forced termination is bounded and logged; cleanup failures remain visible.
- No cancellation shortcut may bypass GitRun authorization or Docker/GitDockRun ownership checks.

## 2. Experimental Smart Scheduler

### Objective

Move beyond scaling based only on the current busy-runner count and queued self-hosted jobs. GitRun should learn from actual workload history, forecast likely demand and startup latency, and recommend or perform bounded adaptations while keeping deterministic policies in charge.

Smart Scheduler must be optional, separately configurable by capability, and disabled by default initially.

### Telemetry: requested versus actual work

Collect enough data to distinguish demand from execution:

- Jobs queued/requested, started, completed, failed, cancelled, skipped where observable, and retried.
- Effective concurrency over time, not only queue length or requested parallelism.
- Queue wait time, runner provisioning/startup time, execution duration, and end-to-end duration.
- Repository, workflow, job name/class, branch/event category, and relevant runner labels, subject to data-minimization rules.
- CPU, memory, disk, and other resource measurements available at job/container level.
- Runner online/offline/busy transitions and container lifecycle events.
- Failures, timeouts, cancellations, and discrepancies between GitHub and Docker inventories.
- Previous predictions, confidence, actions taken, and observed outcomes, so prediction quality can be measured.

Do not collect secrets, environment-variable values, tokens, job payloads, or arbitrary workflow contents as telemetry. Define retention, aggregation, and deletion behavior. Keep metric collection resource-light and avoid unbounded per-job history.

### Workflow Model

Maintain a versioned, machine-readable JSON model derived from observed history. The model is a description of workload characteristics and operational recommendations, not an authority that can override policy.

A model may be scoped by repository, workflow, and job class, with a fallback to broader classes when samples are insufficient. Candidate fields include:

- Schema/model version, generation time, observation window, and sample count.
- Runtime distribution (for example p50/p90/p95), queue-wait and runner-startup distributions.
- Effective concurrency and forecast peak concurrency over named time horizons.
- Resource profiles or distributions derived from measured usage.
- Prediction confidence, model freshness, and known limitations.
- Recommendations such as warm-pool target or prewarming window.
- Prediction-versus-actual history and model quality metrics.

Illustrative shape only; values below are examples, not measured GitRun data:

    {
      "schema_version": 1,
      "model_version": 1,
      "updated_at": "2026-10-15T10:00:00Z",
      "scope": {
        "repository": "owner/repo",
        "workflow": "CI",
        "job_class": "docker-build"
      },
      "observations": {
        "sample_count": 240,
        "window_days": 30
      },
      "runtime": {
        "p50_seconds": 180,
        "p95_seconds": 420
      },
      "demand": {
        "predicted_peak_concurrency": 4,
        "confidence": 0.82
      },
      "recommendations": {
        "prewarm_runners": true,
        "target_warm_pool": 2
      }
    }

Separate four concepts in the implementation and UI:

1. **Observed facts** — what actually happened.
2. **Predictions** — what the model estimates will happen, including uncertainty.
3. **Recommendations** — what action is proposed.
4. **Applied decisions** — what the scheduler actually did and why.

Never let the model silently change hard safety limits, authorization, security policies, or the configured global maximum. Recommendations must be explainable and auditable.

### Prediction and adaptive behavior

Potential capabilities, introduced incrementally:

- Forecast queue arrivals and effective concurrency by time horizon.
- Predict likely job duration and runner startup delay from comparable historical jobs.
- Prewarm runners before predictable peaks.
- Adjust the desired permanent warm pool within operator-defined lower and upper bounds.
- Recommend or apply bounded resource profiles based on measured CPU/memory behavior.
- Detect workload drift, outliers, sustained resource pressure, and degraded prediction accuracy.
- Compare forecast against actual demand and update the model over time.
- Fall back to the classic scheduler when samples are insufficient, the model is stale, or prediction quality degrades.

Forecasting must not be confused with certainty. Real-time observations and resource-pressure safety controls take precedence over predictions. Predicted capacity must never bypass global limits or protected-container accounting.

## 3. Settings and rollout

Add an **Experimental — Smart Scheduler** section. Keep individual capabilities independently switchable:

- **Predictive Analytics:** collect/aggregate telemetry and show forecasts without changing scheduling decisions.
- **Predictive Warm Pool:** prewarm capacity based on forecasts.
- **Adaptive Resource Management:** adjust permitted targets within explicit bounds.
- **Workflow Model:** inspect model scope, freshness, samples, confidence, forecasts, and recommendations.
- **Safety Limits:** show effective minimum/maximum, resource-pressure gates, fallback state, and recent decisions.

Initial default: disabled. Start with observation-only mode, then recommendations, then opt-in automation. Disabling Smart Scheduler must return operation to the classic deterministic scheduler without deleting the historical data by surprise.

Each automatic action should have a reason, source data window, model version, confidence, before/after state, and result. Keep a clear path to disable the feature and revert to classic behavior.

## 4. Delivery sequence

1. **Stabilize and map the existing scheduler.** Verify runner inventory, effective capacity, protected GitDockRun accounting, concurrency, recovery, and global maximum invariants.
2. **Harden Fast Stop independently.** The initial run-level force-cancel path is wired in the dashboard and CLI; add timestamped end-to-end measurements and exercise permission failures, already-completed runs, and cancellation races before release.
3. **Add telemetry and model schema.** Observation-only, bounded resource cost, retention rules, and no automatic scaling changes.
4. **Add passive predictions.** Report accuracy and confidence against real outcomes.
5. **Enable predictive prewarming.** Opt-in, bounded by existing hard limits, with clear fallback.
6. **Evaluate adaptive resource management.** Add only after reliable resource measurements and tests prove that policy and capacity invariants remain enforced.

Keep Fast Stop independent of Smart Scheduler so either feature can ship, be tested, or be disabled without requiring the other.

## 5. Acceptance criteria

### Fast Stop
- [x] Dashboard exposes Fast Stop for active workflow runs with an explicit whole-run confirmation.
- [x] CLI exposes `gitrun fast-stop <owner/repo> <run_id> --yes`; repository allowlist and positive run ID are enforced.
- [x] Uses GitHub's force-cancel run endpoint directly, without waiting for scheduler polling; failures are returned to the caller.
- [x] Does not locally kill or remove runner containers, and explicitly documents that force-cancel affects every job in the selected workflow run.
- [ ] End-to-end timing identifies where latency is spent.
- [ ] API permission failures, already-completed runs, repeated requests, and cancellation races are exercised.
- [ ] p50/p95/max latency is recorded for a documented test workload; no fixed-latency promise is made before this evidence exists.

### Smart Scheduler
- Requested jobs and actually executed jobs are represented separately.
- Effective concurrency and resource usage are derived from observed events/metrics, not inferred from queue count alone.
- Workflow Model JSON is schema-versioned, bounded, validated, and inspectable.
- Predictions are evaluated against actual outcomes and include confidence/sample information.
- Insufficient or stale data triggers classic behavior.
- All automated actions obey global capacity, resource-pressure, security, and ownership policies.
- Every automatic action is explainable and auditable.

## Non-goals

- Replacing deterministic safety/policy checks with an opaque model.
- Claiming that GitRun can force an internal GitHub Actions runner condition evaluation without a supported mechanism.
- Guaranteeing a fixed cancellation latency independent of GitHub API/network behavior or workload signal handling.
- Allowing predictive logic to override hard limits or security controls.


## v1.5.0 review boundary

Fast Stop implementation and its included tests are evaluated by the current PR checks; any behavior not exercised by those tests remains unverified rather than implicitly accepted. The Experimental Smart Scheduler remains a proposal and must not be represented as a shipped, production-ready scheduling capability unless separately implemented, tested and approved. See the [refactor status tracker](v1.5.0-refactor-status.md).
