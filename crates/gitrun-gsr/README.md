# `gitrun-gsr`

GitSecureRun security layer. It contains the standalone watchdog, runner-side command/policy enforcement, Docker/process observation, workflow validation and the workflow-facing Git*Run API launcher.

The v1.5.0 CI suite includes GSR watchdog lifecycle/failure detection and security-hardening regressions. These automated checks do not imply that every deployment-specific process supervision or host policy configuration has been exercised; consult the [security model](../../docs/SECURITY_MODEL.md) and [status tracker](../../docs/v1.5.0-refactor-status.md).
