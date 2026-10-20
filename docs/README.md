# GitRun documentation

Architecture, operator guidance, API contracts and security documentation for the GitRun 1.5.1 source line.

## Current release notes

- [GitRun 1.5.1 — recovery and documentation notes](v1.5.1-release-notes.md) — protected state, guarded migration of legacy 1.5.0 installations, crate/product version boundaries, Smart Scheduler retention and deployment verification.
- [Architecture](ARCHITECTURE.md) — runtime components and trust boundaries.
- [Security model](SECURITY_MODEL.md) — explicit trust assumptions, hardening and known limits.
- [Workflow API](GITRUN_API.md) and [IPC](IPC.md) — request identity, transport and authorization boundaries.
- [VM-backed runners](VM.md) — supported backend behavior and operator requirements.
- [Dashboard redesign roadmap](dashboard-redesign-roadmap.md) — UI goals, implemented behavior and remaining environment-specific checks.
- [Fast Stop / Experimental Smart Scheduler](fast-stop-and-experimental-smart-scheduler.md) — operator-triggered cancellation, Kitty forecasting, retention, and currently inactive automation.

## Historical audit records

- [v1.5.0 audit summary](v1.5.0-refactor-status.md) — concise record of the merged refactor, the checks run at that historical source commit, and remaining environment-specific limits.
- [Rust migration notes](RUST_MIGRATION.md) — runtime migration background and component map.

## Evidence rule

A passing CI workflow establishes only the checks executed on the recorded commit. It does not prove that no GitHub Security alerts remain, that desktop authorization and installation transitions work on every machine, or that recovery has been validated on every legacy installation or customized runner image. Confirm the exact checks for the target commit and follow the host-level validation steps in the 1.5.1 release notes.

The root `version.txt` defines the product/source version. It does not by itself mean that an official release artifact has been published; use [GitHub Releases](https://github.com/Vider06/GitRun/releases) to verify published artifacts.
