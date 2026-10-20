# GitRun

[![CI](https://github.com/Vider06/GitRun/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Vider06/GitRun/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Vider06/GitRun)](https://github.com/Vider06/GitRun/releases)
[![License](https://img.shields.io/github/license/Vider06/GitRun)](LICENSE)

**GitRun** is a Rust-native control plane for self-hosted GitHub Actions runners. It brings runner autoscaling, VM-backed targets, encrypted secrets, recovery, security policy and a desktop operator dashboard into one system—without requiring Kubernetes.

It is built for people who want control over their own runner infrastructure without giving every workflow unrestricted access to the host. Bring a dedicated Linux machine, Docker and the repositories you trust; GitRun handles the runner lifecycle while keeping privileged operations behind an explicit API and policy checks.

> **Version note:** this branch targets source version **1.5.1**. That is not a promise that a 1.5.1 release artifact is published yet; use [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published builds.

## Architecture at a glance

```text
GitHub Actions
     |
     v
managed runner container
     |
     | Git*Run API client
     v
/run/gitrun/api.sock
     |
     v
GitRun host service / GSR authorization
     |
     +--> gitrun-exe execution boundary
     +--> local Docker host
     +--> VM Docker daemon (KVM/libvirt or VirtualBox)
     +--> GitVault
     +--> scheduler / runner lifecycle
     +--> dashboard + persistent state
```

The workflow-facing Git*Run API is a **closed, explicit API surface**, not a generic shell wrapper. Requests are validated against the API/operation contract, checked against effective repository policy and GSR command/resource policy, and only then handed to the internal execution layer.

See:
- [GitRun 1.5.1 release and recovery notes](docs/v1.5.1-release-notes.md) — protected-state diagnostics, guarded legacy installation repair, version boundaries and deployment verification.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — component and data-flow architecture.
- [docs/GITRUN_API.md](docs/GITRUN_API.md) — Git*Run API reference and authorization boundary.
- [docs/VM.md](docs/VM.md) — VM-backed runner architecture and lifecycle modes.
- [docs/IPC.md](docs/IPC.md) — local API socket and GSR/executor trust boundary.
- [docs/SECURITY_MODEL.md](docs/SECURITY_MODEL.md) — security model, enforcement and limitations.

## Core capabilities

- Docker-based Linux self-hosted runners with autoscaling and recovery.
- VM-backed runner targets through KVM/libvirt or VirtualBox.
- Logic Containers rules that route jobs by GitHub labels to a local Docker host or named VM.
- Closed Git*Run APIs: `GitVaultRun`, `GitDockRun`, `GitSaveRun`, `GitRegisterRun`, `GitInstallRun`, `GitReadRun`, `GitWriteRun`, `GitVerifyRun` and `GitStatusRun`.
- Unix-socket API transport at `/run/gitrun/api.sock` by default, configurable with `GITRUN_API_SOCKET`.
- GSR authorization and command-policy enforcement before privileged execution.
- Authenticated GSR-to-executor envelopes with HMAC-SHA256, timestamps, nonces and replay protection.
- GitVault AES-256-GCM encrypted-at-rest secrets with global, group and repository scopes.
- Per-runner cache isolation by default; broader repository/global cache scopes are explicit trust decisions.
- Dedicated GitRun Docker networking with rejection of ordinary `bridge`, `host` and `container:*` network modes.
- VM endpoint health verification, Windows Docker TLS validation and operator-controlled snapshot rollback primitives.
- Host resource-pressure backpressure for CPU, memory and configurable critical filesystems.
- Optional Kitty statistical Smart Scheduler: bounded workload history, evaluated demand forecasts, and explicitly gated Predictive Warm Pool; disabled by default and subordinate to deterministic reconciliation.
- Tauri 2 operator dashboard for configuration, health, runner pools, VM definitions, Logic Containers and security controls.
- Versioned updater with checksum verification, signed-manifest verification and rollback.

## Security boundary

GitRun-managed Linux runners do **not** receive the host Docker socket by default. Direct Docker-socket access is a per-repository compatibility opt-in. When enabled, it grants workflow code host-level Docker authority, not a normal unprivileged container capability.

GSR adds defense-in-depth: container hardening, command-policy enforcement and external process supervision. Git*Run API operations additionally require the caller's authoritative repository, workflow run, job and runner identity. Dock operations require the exact persisted runner/container binding and immutable container identity, with revalidation before privileged execution.

Do not connect untrusted repositories to a host that contains sensitive workloads. Read [SECURITY.md](SECURITY.md) and [docs/SECURITY_MODEL.md](docs/SECURITY_MODEL.md) before deployment.

## Installation

### Linux

```bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
```

First-run graphical setup is available through the installed application. Terminal setup is available with:

```bash
gitrun setup --terminal
```

### macOS

```bash
./scripts/install-macos.sh
```

### Windows

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\scripts\install-windows.ps1
```

For installation prerequisites and platform-specific details, see [the installation scripts](scripts/README.md) and [release process](release/README.md).

## Configuration

The reference environment file is [config/config.example.env](config/config.example.env). GitRun supports PAT and GitHub App authentication.

Important runtime state includes:

- `GITRUN_STATE_DIR` — persistent scheduler/dashboard state.
- `GITRUN_LOG_DIR` — runtime logs.
- `GITRUN_VAULT_DIR` — GitVault storage.
- `GITRUN_RUNNER_IMAGE` — default runner image.
- `GITRUN_RUNNER_HOME_BACKEND` — runner home storage backend (`volume` is the hardened default; `tmpfs` requires an explicitly writable root).
- `GITRUN_RUNNER_ROOTFS_READ_ONLY` — mount Linux runner root filesystems read-only (default: `true`).
- `GITRUN_RUNNER_WINDOWS_HYPERV_ISOLATION` — require Hyper-V isolation for Windows runner containers (default: `true`).
- `GITRUN_RUNNER_NETWORK` — dedicated Docker network used by managed runners.
- `GITRUN_RESOURCE_PRESSURE_PATHS` — semicolon-separated critical filesystem paths used by resource-pressure backpressure.
- `GITRUN_GSR_*` — GSR hardening, command policy and workflow validation.
- `GITRUN_API_SOCKET` — workflow-facing API socket path; defaults to `/run/gitrun/api.sock`.

VM definitions are stored in:

```text
{GITRUN_STATE_DIR}/vm-configs.json
```

Logic Containers rules are stored in the corresponding persistent state and are matched against GitHub job labels. VM and Logic Container configuration is shared with the Tauri dashboard.

## Autoscaling and runner lifecycle

For each repository GitRun reconciles desired capacity from minimum runners, busy runners and queued self-hosted jobs, capped by the configured maximum. `min_runners=0` is supported when zero idle capacity is desired.

Linux runners use the local Docker daemon. A Logic Containers rule can instead target a named VM. Ephemeral runners are not configured with Docker's `unless-stopped` restart policy, so an exited ephemeral runner is not silently recreated by Docker.

## VM support

VM-backed execution supports:

- **KVM/libvirt** as the preferred hypervisor.
- **VirtualBox** as the fallback when KVM is unavailable.
- Linux or Windows guest images, depending on the supplied base image.
- Docker TLS for Windows VM-backed endpoints where required.
- Docker endpoint health verification before a VM becomes routable.
- Operator-controlled snapshot create/restore primitives, including the `EphemeralSnapshotRollback` lifecycle mode.

The full VM recreation/ephemeral-VM lifecycle remains deployment/operator dependent. GitRun does not distribute Windows images, silently install hypervisors, or configure Docker inside an arbitrary guest image. See [docs/VM.md](docs/VM.md).

## Git*Run API

The API commands are intentionally explicit:

| API | Purpose |
|---|---|
| `GitVaultRun` | Scoped secret read/write/list/exists/delete |
| `GitDockRun` | Connect to a job runner; read/write/execute/melt docked containers; mount/unmount approved host paths on local Linux Docker |
| `GitSaveRun` | Save a workflow file or retrieve named logs |
| `GitRegisterRun` | Register a workflow path for the current run |
| `GitInstallRun` | Install/remove/update an allowed package |
| `GitReadRun` | Read an allowed runner filesystem path |
| `GitWriteRun` | Write an allowed runner filesystem path |
| `GitVerifyRun` | Calculate a SHA-256 digest for an allowed path |
| `GitStatusRun` | Query GitRun status |

The host does not trust workflow-visible identity claims by themselves. It binds the request to the current in-progress GitHub job and Docker runner, verifies authoritative workflow/run information, and applies repository/resource policy before creating an authorized operation.

See [docs/GITRUN_API.md](docs/GITRUN_API.md) for the operation grammar and security rules.

## CLI and dashboard

Common CLI commands:

```text
gitrun setup
gitrun setup --terminal
gitrun connect owner/repository
gitrun config
gitrun desired <min> <max> <busy> <queued>
gitrun doctor
gitrun update [manifest-url]
gitrun dashboard
gitrun rollback <backup-path>
gitrun cat
```

`gitrun cat` is the terminal easter egg and uses one JSON animation source with adaptive terminal rendering.
`gitrun fast-stop <owner/repo> <workflow-run-id> --yes` requests GitHub force-cancellation for the selected workflow run. It cancels the run, not a single job, and does not itself prove that runner processes have exited.

## Repository map

The repository map is intentionally compact; the documents linked above are the primary starting points.

At the top level:

- `.github/` — CI, security automation, templates and reusable action code.
- `assets/` — shared GitRun artwork and application icons.
- `config/` — configuration examples.
- `crates/` — Rust workspace crates and their UI resources.
- `docker/` — manager and runner container definitions.
- `docs/` — architecture, API, VM, IPC and security documentation.
- `packaging/` — desktop/package integration.
- `release/` — release manifests and schemas.
- `scripts/` — installation, build and verification tooling.
- `systemd/` — Linux service units.

## Development

The source version targeted by this branch is **1.5.1** (see `version.txt`). This does not imply that a 1.5.1 release artifact has been published. Start with:

```bash
cargo fmt --all --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Changes affecting Docker, workflows, shell, PowerShell, packaging, Tauri or release artifacts should also run the corresponding repository checks.

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and [SUPPORT.md](SUPPORT.md).

## License

GitRun is distributed under the Apache License 2.0.
