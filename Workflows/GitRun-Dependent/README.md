# GitRun-Dependent workflows

Workflow content that may consume GitRun-provided capabilities, artifacts, labels, or services.

## Kitty Dispatch planner

These files live in the Premade artifact library, not in GitHub's automatically scanned workflow directory. Copy/install them into the consuming repository under `.github/workflows/` (for example, `kitty-dispatch-plan.yml` and `kitty-dispatch-example.yml`) before calling them; GitHub will not execute a workflow straight from the `Workflows/GitRun-Dependent/` library path.

- `kitty-dispatch-plan.yml` is a reusable workflow that reports `mode=kitty` only after the local GitRun controller verifies opt-in settings and issues a unique per-occurrence runner route.
- The reusable workflow separately emits `controller_online`, `kitty_online` (true/false/unknown from validated Kitty settings), `mode`, controller status, fallback reason, and the JSON label array. `kitty_online=true` does not imply that Kitty routed the job: Shadow mode, opt-in settings, or an unknown workload profile may still select `mode=github`.
- `kitty-dispatch-example.yml` shows a caller receiving the JSON `runs_on` output and passing it to `runs-on: ${{ fromJSON(needs.plan.outputs.runs_on) }}`.
- The plan job must run on a GitRun-managed Linux runner that has the `gitrun-control` label, access to the local GitRun API socket, and a compatible GitRun build. Append `gitrun-control` to the existing `GITRUN_RUNNER_LABELS` value (for example, keep existing labels and add `,gitrun-control`) so at least one compatible planner runner is available.
- Enable **Kitty**, **aggregate telemetry**, **Adaptive Resource Management**, and **Kitty Dispatch** in the GitRun settings; switch **Shadow mode off** when you're ready for active routing. Authorize `GitDispatchRun/Plan` in the repository's GSR API policy as a separate security decision.
- Set `target_workflow` to the exact `.github/workflows/...` path and `target_job` to the stable historical job identifier/name expected by Kitty. `target_labels` must contain ordinary compatible GitHub runner labels, not any reserved `gitrun-job-*` or `gitrun-route-*` label.
- The workflow explicitly reports `mode` (`kitty` or `github`), `kitty_online` (true only when a validated Kitty route is issued), `status`, `reason`, and the selected `runs_on` array in its job outputs and step summary. Kitty-disabled, shadow, unknown-profile, API-unavailable, and GSR-denied cases use ordinary labels and report GitHub fallback rather than claiming that Kitty routed the job.
- A live planner runner is a prerequisite for any fallback decision. If no compatible `gitrun-control` runner can start the planning job, GitHub keeps that job queued; no workflow code can detect Kitty's state until the planner actually runs.

GitHub remains the final scheduler: the unique route label makes the matching GitRun-managed runner eligible, but it is not a GitHub API that directly pins a job to a runner ID. One-use routed runners are ephemeral; the scheduler verifies the actual job/container binding. VM-backed routing and pre-existing queued jobs with different `runs-on` labels are not rewritten by this workflow.
