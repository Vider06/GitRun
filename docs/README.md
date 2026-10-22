# GitRun documentation

Start with the [main README](../README.md) for a readable overview of GitRun's features, setup path and recommended first steps.

## Understand GitRun

- [Architecture](ARCHITECTURE.md) — runtime components, request flow, state and trust boundaries.
- [Git*Run API reference](GITRUN_API.md) — exact workflow API commands, arguments and authorization requirements.
- [IPC and socket architecture](IPC.md) — local socket transport and authenticated executor protocol.
- [VM-backed runners](VM.md) — KVM/libvirt, VirtualBox, Logic Containers, TLS and lifecycle limits.
- [Security model](SECURITY_MODEL.md) — enforcement details, deployment assumptions and residual risks.
- [Kitty, Adaptive Resources and Fast Stop](SMART_SCHEDULER.md) — forecasts, bounded history, queue-aware evaluation, Future Mapping, Think, optional capacity automation and cancellation semantics.

## Version history

- [Release history](RELEASE_HISTORY.md) — 1.5.0 refactor baseline, 1.5.1 recovery migration and historical verification boundaries.
- [GitRun 1.6.1 fixes](v1.6.1-fixes.md) — runtime/dashboard recovery, GitHub permission diagnostics and package lifecycle.
- [GitRun 1.6.2 fixes](v1.6.2-fixes.md) — protected security-settings reads in the dashboard.

## Evidence and deployment

CI results apply only to the commit and checks they report. A passing workflow does not prove that no security alerts remain or that privileged desktop authorization, real VM provisioning/recovery, custom runner-image migration and target-host repair have all been tested. Check the exact workflow results and use [GitHub Releases](https://github.com/Vider06/GitRun/releases) to distinguish source versions from published artifacts.
