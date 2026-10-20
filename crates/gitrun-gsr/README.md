# `gitrun-gsr`

GitSecureRun security layer. It contains the standalone watchdog, runner-side command/policy enforcement, Docker/process observation, workflow validation and the workflow-facing Git*Run API launcher. The `src/bin/` entry points are `gitrun-api` (workflow-facing transport client) and `gitrun-gsr-agent` (runner-side enforcement).

PR #83's source HEAD passed the GSR watchdog lifecycle/failure-detection tests and security-hardening regression suite; those results are scoped to that 1.5.0 source commit. PR #103 added recovery behavior that restores the packaged `gitrun.service` and `gitrun-gsr.service` contracts, including their hardening directives. Host-specific process supervision and repair still need validation on the target host. See the [security model](../../docs/SECURITY_MODEL.md), [1.5.1 release notes](../../docs/v1.5.1-release-notes.md) and [historical status report](../../docs/v1.5.0-refactor-status.md).
