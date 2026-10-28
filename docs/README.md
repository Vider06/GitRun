# GitRun documentation

Start with the [main README](../README.md) for a readable overview of GitRun's features, setup path and recommended first steps.

## Understand GitRun

- [Architecture](ARCHITECTURE.md) — runtime components, request flow, state and trust boundaries.
- [GTUU permanent-runner maintenance](../README.md#first-steps) — read-only checks, validated forced image rebuilds, safe runner replacement and automatic sudo authorization.\n- [Terminal management console](../crates/gitrun-cli/README.md#terminal-management-console) — keyboard controls, headless operations, validated writes and responsive mascot behavior.
- [JSON integrity and recovery](../crates/gitrun-cli/README.md#json-integrity-and-recovery) — Doctor diagnostics, conservative repair, local-backup restoration and signed Main Library restoration.
- [Recovery component](../crates/gitrun-recovery/README.md) — protected-state behavior, safe recovery boundaries and reference-library handling.
- [Dashboard host CPU history](../crates/gitrun-dashboard-tauri/README.md#host-cpu-history) — what the recent CPU graph measures and what its bounded, process-local history does not prove.
- [Git*Run API reference](GITRUN_API.md) — exact workflow API commands, arguments and authorization requirements.
- [IPC and socket architecture](IPC.md) — local socket transport and authenticated executor protocol.
- [VM-backed runners](VM.md) — KVM/libvirt, VirtualBox, Logic Containers, TLS and lifecycle limits.
- [Security model](SECURITY_MODEL.md) — enforcement details, deployment assumptions and residual risks.
- [Kitty, Adaptive Resources and Fast Stop](SMART_SCHEDULER.md) — forecasts, bounded history, queue-aware evaluation, Future Mapping, Think, optional capacity automation and cancellation semantics.

## Current source line

The current source/product version on this branch is **<!-- gitrun-version:start -->1.8.16<!-- gitrun-version:end -->** (`version.txt`). Current-version markers are maintained by `scripts/sync-markdown-version.py`. PR CI validates marker pairing and required coverage; the release workflow refreshes marker values and strictly checks them before packaging, then opens a docs-only pull request when `main/version.txt` matches the selected release version. It does not push directly to `main`. This page describes the source tree and does not by itself assert that a signed release artifact has been published; verify [GitHub Releases](https://github.com/Vider06/GitRun/releases).

## Version history

- [GitRun 1.8.9 source-line notes](v1.8.9-fixes.md) — isolated Main Library restoration, protected JSON diagnostics and version alignment.
- [GitRun 1.8.16 source-line notes](v1.8.16-fixes.md) — GTUU checks, forced local runner-image rebuilds, permanent-runner replacement and privileged delegation.\n- [GitRun 1.8.15 source-line notes](v1.8.15-fixes.md) — fingerprinted local runner-image rebuilding, runtime policy bootstrap validation and GTUU reconciliation.
- [GitRun 1.8.14 source-line notes](v1.8.14-fixes.md) — acknowledged Windows/Linux Logs copying with a file-save fallback and consistent GSR runner bootstrap.
- [GitRun 1.8.12 source-line notes](v1.8.12-fixes.md) — copy Logs to a native clipboard where available and forward OSC 52 correctly through tmux.
- [GitRun 1.8.11 source-line notes](v1.8.11-fixes.md) — remove the immediate Main Library restore timeout and safely recover the workflow API socket path.
- [GitRun 1.8.10 source-line notes](v1.8.10-fixes.md) — Docker's built-in seccomp profile handling and Main Library restore diagnostics.
- [GitRun 1.8.0 source-line notes](v1.8.0-fixes.md) — CLI management console, child-process output handling and version alignment.
- [Release history](RELEASE_HISTORY.md) — earlier release milestones and historical verification boundaries.
- [GitRun 1.6.1 fixes](v1.6.1-fixes.md) — runtime/dashboard recovery, GitHub permission diagnostics and package lifecycle.
- [GitRun 1.6.2 fixes](v1.6.2-fixes.md) — protected security-settings reads in the dashboard.

## Evidence and deployment

CI results apply only to the commit and checks they report. A passing workflow does not prove that no security alerts remain or that privileged desktop authorization, real VM provisioning/recovery, custom runner-image migration and target-host repair have all been tested. Check the exact workflow results and use [GitHub Releases](https://github.com/Vider06/GitRun/releases) to distinguish source versions from published artifacts.
