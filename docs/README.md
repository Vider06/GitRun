# GitRun documentation

Use this index to find the maintained technical guide for a GitRun subsystem. Start with the [main README](../README.md) for an overview, installation steps and first-run workflow.

## Operator guides

- [Architecture](ARCHITECTURE.md) — runtime components, request flow, state ownership and trust boundaries.
- [Terminal management console](../crates/gitrun-cli/README.md#terminal-management-console) — keyboard navigation and headless administration.
- [JSON integrity and recovery](../crates/gitrun-cli/README.md#json-integrity-and-recovery) — Doctor diagnostics, conservative repair, local backups and signed Main Library restoration.
- [Recovery component](../crates/gitrun-recovery/README.md) — protected state and recovery boundaries.
- [Dashboard host CPU history](../crates/gitrun-dashboard-tauri/README.md#host-cpu-history) — what the recent CPU graph measures and its limits.
- [VM-backed runners](VM.md) — KVM/libvirt, VirtualBox, Docker endpoints, TLS and lifecycle constraints.
- [Kitty Smart Scheduler, adaptive resources and Fast Stop](SMART_SCHEDULER.md) — forecasts, bounded history, optional capacity automation and cancellation semantics.

## Technical references

- [Git*Run workflow API](GITRUN_API.md) — operation names, arguments, authorization and scope.
- [IPC and socket architecture](IPC.md) — local socket transport and executor authentication.
- [Security model](SECURITY_MODEL.md) — enforcement details, assumptions and residual risks.
- [Release history](RELEASE_HISTORY.md) — consolidated source-line history and historical verification boundaries.
- [Release metadata and Main Library artifact](../release/README.md) — version synchronization and signed reference-library contents.

## Source version and releases

The current source/product version is <!-- gitrun-version:start -->1.8.21<!-- gitrun-version:end -->, recorded in the root version.txt and mirrored in marked documentation references. This page describes source-tree documentation; it does not establish that a signed release artifact is available. Use [GitHub Releases](https://github.com/Vider06/GitRun/releases) to find published packages.

## Validation boundaries

Historical changes and test results describe the source commit on which they were recorded. They are not proof of current behavior on every host. Privileged desktop flows, host repair, VM provisioning and recovery, customized runner-image migration, and release-signing configuration require validation in the target environment.
