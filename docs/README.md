# GitRun documentation

Architecture, operator guidance, API contracts and security documentation for the **1.6.2 source line**.

## Current source-line notes

- [GitRun 1.6.2 fixes](v1.6.2-fixes.md) — dashboard reads of protected GitRun security settings, narrowly scoped administrator authorization, and the failure behavior when access cannot be granted.
- [GitRun 1.6.1 fixes](v1.6.1-fixes.md) — earlier dashboard/runtime recovery and package-lifecycle fixes.
- [GitRun 1.5.1 recovery notes](v1.5.1-release-notes.md) — historical explanation of the protected-state and legacy runtime/systemd repair work introduced after 1.5.0.

## Architecture and contracts

- [Architecture](ARCHITECTURE.md) — components, data flow, persistent state, and trust boundaries.
- [Git*Run API](GITRUN_API.md) — workflow-facing operation grammar and authorization requirements.
- [IPC and socket architecture](IPC.md) — Unix socket transport and internal GSR/executor authentication.
- [Security model](SECURITY_MODEL.md) — enforcement boundaries, deployment assumptions, and residual risks.
- [VM-backed runners](VM.md) — hypervisors, guest endpoints, routing, TLS, and lifecycle limits.
- [Dashboard redesign roadmap](dashboard-redesign-roadmap.md) — dashboard information architecture and remaining environment-specific validation.
- [Fast Stop and Kitty Smart Scheduler](fast-stop-and-experimental-smart-scheduler.md) — cancellation semantics, forecasting, retention, and opt-in controls.

## Historical audit records

- [v1.5.0 audit summary](v1.5.0-refactor-status.md) — the merged refactor's scope and commit-specific verification record.

## Evidence rule

A passing CI workflow establishes only the checks executed on the recorded commit. It does not prove that no GitHub Security alerts remain, that desktop authorization and installation transitions work on every machine, or that recovery has been validated on every legacy installation or customized runner image. Check the exact CI results for the target commit and perform host-level validation for privileged desktop flows, VM provisioning/recovery, and deployment-specific migrations. A source version or merged pull request is not evidence that a signed release artifact has been published; check [GitHub Releases](https://github.com/Vider06/GitRun/releases).
