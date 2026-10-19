# GitRun documentation

Project architecture, operator guidance, API contracts and security documentation for **GitRun 1.5.0** on PR [#83](https://github.com/Vider06/GitRun/pull/83).

## Core references

- [Architecture](ARCHITECTURE.md) — runtime components and trust boundaries.
- [Security model](SECURITY_MODEL.md) — explicit trust assumptions, hardening and known limits.
- [Workflow API](GITRUN_API.md) and [IPC](IPC.md) — request identity, transport and authorization boundaries.
- [VM-backed runners](VM.md) — supported backend behavior and operator requirements.
- [Contract inventory](v1.5.0-contract-inventory.md) — requirements, source references and test evidence.
- [Refactor status and final report](v1.5.0-refactor-status.md) — phase tracker, verification evidence, open limitations and release decision.
- [Dashboard redesign roadmap](dashboard-redesign-roadmap.md) — UI goals and implementation status.
- [Fast Stop / Experimental Smart Scheduler](fast-stop-and-experimental-smart-scheduler.md) — implemented cancellation work versus proposed experimental scheduling.

## Evidence rule

A passing CI workflow establishes only the checks actually executed on the recorded commit. It does not prove zero open alerts in GitHub Security, real desktop authorization behavior, real KVM/VirtualBox provisioning, or migration behavior for every operator-customized runner image. The status tracker records those distinctions and must be read alongside the contract inventory.

**Version target:** 1.5.0. **Tracking PR:** #83. All refactor changes belong on branch `refactor/v1.5.0-total-cleanup`; do not merge or modify `main` as part of this work.
