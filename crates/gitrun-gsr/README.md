# `gitrun-gsr`

GitSecureRun security layer. It contains the standalone watchdog, runner-side command/policy enforcement, Docker/process observation, workflow validation and the workflow-facing Git*Run API launcher.

PR #83's source HEAD passed the GSR watchdog lifecycle/failure-detection tests and security-hardening regression suite. Host-specific process supervision and the deployment's systemd policy still depend on the installed unit and host configuration; see the [security model](../../docs/SECURITY_MODEL.md) and [status report](../../docs/v1.5.0-refactor-status.md).
