# GitRun

[![CI](https://github.com/Vider06/GitRun/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Vider06/GitRun/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Vider06/GitRun)](https://github.com/Vider06/GitRun/releases)
[![License](https://img.shields.io/github/license/Vider06/GitRun)](LICENSE)

**Your self-hosted GitHub Actions runners, under your control.**

GitRun is a Rust-native control plane for self-hosted GitHub Actions runners. It provisions and scales Docker runners, can route selected workloads to virtual machines, and brings secrets, security policy, recovery and day-to-day operations into one desktop dashboard and CLI.

It is designed for developers and teams who want to use their own runner infrastructure without turning every workflow into an unrestricted host administrator. **No Kubernetes cluster is required.**

> **Source version:** this branch targets `1.8.1`. A source version or passing CI does not mean a signed release artifact is available. For installable releases, use [GitHub Releases](https://github.com/Vider06/GitRun/releases).

## What can GitRun do?

<details open>
<summary><strong>Run and scale your own CI runners</strong></summary>

- Connect GitHub repositories and manage their self-hosted runner pools.
- Reconcile runner capacity from minimum/maximum settings, busy runners and queued jobs.
- Run Linux workloads in managed Docker containers, with ephemeral runner lifecycle and recovery.
- Apply CPU, memory, PID and host-resource-pressure limits before creating more work.
- Keep runner caches isolated by default; repository-wide and global sharing are deliberate trust choices.
- Use prepared runner profiles for a lightweight minimum image or a fuller development workbench.

</details>

<details>
<summary><strong>Route workloads to the right environment</strong></summary>

- Use **Logic Containers** to match GitHub job labels to a local Linux Docker host or a named VM.
- Use KVM/libvirt when available, with VirtualBox as a supported fallback.
- Route Windows workloads only to configured VM-backed Docker endpoints; GitRun does not silently run them on the local Linux daemon.
- Validate guest Docker endpoints before they become routable. Windows VM endpoints can use mutually authenticated Docker TLS.
- Use operator-managed snapshot rollback primitives where the selected VM lifecycle supports them.

GitRun does not ship guest operating-system images or silently install hypervisors. The operator supplies and secures the VM images, guest Docker daemon and credentials. Full VM recreation remains deployment-dependent.

</details>

<details>
<summary><strong>Give workflows useful capabilities without a generic host shell</strong></summary>

The workflow-facing **Git*Run API** is a closed set of named operations—not an arbitrary host-command endpoint.

| API | What it provides |
|---|---|
| `GitVaultRun` | Read, write, list, check and delete authorized secrets |
| `GitDockRun` | Bind to the current runner; perform policy-checked container operations and supported local-Linux mounts |
| `GitSaveRun` | Save approved workflow files or retrieve named logs |
| `GitRegisterRun` | Register workflow paths for a run |
| `GitInstallRun` | Policy-gated package install, removal and update inside the runner |
| `GitReadRun` / `GitWriteRun` | Read and write approved runner paths |
| `GitVerifyRun` | Calculate a SHA-256 digest for an approved file |
| `GitStatusRun` | Query GitRun status |
| `GitDispatchRun --plan` | Request an optional Kitty Dispatch plan with an explicit GitHub fallback |

Requests are checked against the current runner/container identity, authoritative GitHub job state and effective repository/resource policy. GitDockRun operations additionally require a persisted binding to the exact runner container. Complex shell scripts should remain ordinary workflow steps and pass through GSR's command supervision.

</details>

<details>
<summary><strong>Protect secrets, containers and privileged operations</strong></summary>

- **GitVault:** AES-256-GCM encryption at rest, with global, group and repository scopes.
- **GitSecureRun (GSR):** command/resource policy, workflow validation, security events and a separate watchdog process. An optional, consent-gated `zizmor` integration can add static analysis for GitHub Actions workflows.
- **Protected execution:** a local Unix API socket, authoritative workflow checks, authenticated GSR-to-executor messages, HMAC-SHA256, random nonces, timestamps and replay protection.
- **Container hardening:** read-only Linux root filesystem by default, constrained capabilities, no-new-privileges, resource limits and a dedicated runner network.
- **Safer filesystem access:** approved path prefixes, symlink/hard-link checks and bounded file operations.
- **Controlled host mounts:** GitDockRun mounts are a separately gated capability for local Linux Docker; policy, destination capability and runner identity must all permit the operation.
- **Signed updates:** release-manifest verification, checksums and rollback support.

**Important:** direct Docker-socket access for workflow runners is disabled by default. Enabling it gives workflow code host-level Docker authority; only enable it for repositories and contributors you fully trust. GitRun is intended for infrastructure you administer, not as an open runner service for arbitrary public repositories.

</details>

<details>
<summary><strong>Forecast demand and adapt resource allocation — optional Kitty features</strong></summary>

**Kitty Smart Scheduler** is a bounded statistical add-on, not an LLM and not a replacement for the deterministic scheduler.

- Record bounded five-minute aggregate observations and forecast near-term runner demand.
- Compact older history into weekly, four-week and yearly summaries instead of growing raw JSON forever.
- Evaluate forecasts against later observations and expose a heuristic readiness score.
- Optionally recommend a predictive warm pool, only after separate opt-in, fresh history, acceptable forecast quality and explicit exit from shadow mode.
- Optionally estimate CPU/RAM needs for new local Docker runners from observed workload samples, while respecting operator ceilings and reserving host headroom.
- Optionally trigger Smart Fast Stop under explicit policy and authorization.
- Evaluate Erlang-C queue-wait estimates and deterministic FCFS replay through the bounded queueing module; these are evaluation tools, not an automatic live scaling policy.
- Optionally enable **Future Mapping**, a private rolling one-hour projection with twelve five-minute demand/resource buckets and typed, short-lived revision patches.
- Optionally enable **Think**, which compares independent history windows and withholds predictive growth when coverage is incomplete or the estimates disagree. Both capabilities are experimental, disabled by default and remain subordinate to the deterministic scheduler, operator limits and exact-revision GSR authorization.

Kitty is disabled by default; resource adaptation and automatic cancellation are opt-in. Forecast scores are heuristics, not calibrated probabilities. Adaptive resource management does not resize existing containers, and VM-backed workloads are deferred while per-VM capacity probing is unavailable. Fast Stop requests cancellation of an entire GitHub workflow run; an accepted request does not prove every process has exited.

See [Kitty, Adaptive Resources and Fast Stop](docs/SMART_SCHEDULER.md).

</details>

<details>
<summary><strong>Operate GitRun from a desktop dashboard or CLI</strong></summary>

The native **Tauri 2 dashboard** brings together:

- Repository and runner-pool configuration.
- Runner, service and host-health visibility.
- Logic Containers and VM definitions.
- GitVault secret metadata and scopes.
- GSR security controls and event visibility.
- Setup diagnostics, protected-state recovery and repair actions.
- Configuration, update and lifecycle controls.

The CLI supports setup, repository connection, diagnostics, update checks, dashboard launch, rollback and terminal utilities. `gitrun manage` opens a keyboard-driven terminal management console for headless servers; it reuses the dashboard's validated backend operations instead of maintaining a second settings implementation. The CLI mascot is shown by default for normal commands and can be suppressed explicitly with `--no-cat`; internal child-process launches suppress it to keep terminal output clean. The scheduler continues to run when the dashboard is closed. The dashboard does not treat inaccessible protected files as proof of corruption and does not relax file permissions as a workaround.

</details>

## Get started

### Requirements

For the standard Linux host deployment, prepare a dedicated Linux machine with Docker and a GitHub credential authorized for the repositories and runner operations you intend to manage. Keep the host and Docker API private. VM-backed workflows additionally require a supported hypervisor, operator-provided guest images and a configured guest Docker endpoint.

### Install on Linux

```bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
```

Then launch the first-run setup, or configure from a terminal:

```bash
gitrun setup --terminal
```

Install scripts are also provided for macOS and Windows; see [scripts/README.md](scripts/README.md) for the exact platform paths and constraints. Use [GitHub Releases](https://github.com/Vider06/GitRun/releases) when you want a published artifact rather than a checkout of the source branch.

### First steps

1. Run setup and configure GitHub authentication.
2. Connect a repository with `gitrun connect OWNER/REPOSITORY`.
3. Set runner-pool minimum/maximum and resource limits to match your host.
4. Review the repository's trust level and keep direct Docker-socket access disabled unless it is genuinely required.
5. Start with the deterministic scheduler. Enable optional Kitty telemetry first, observe forecasts in shadow mode, and only then consider capacity automation.

Useful CLI commands:

```text
gitrun setup
gitrun setup --terminal
gitrun connect OWNER/REPOSITORY
gitrun config
gitrun doctor
gitrun manage
gitrun dashboard
gitrun update --check
gitrun fix-update
gitrun fast-stop OWNER/REPOSITORY WORKFLOW_RUN_ID --yes
gitrun rollback BACKUP_PATH
gitrun cat
```

The `fix-update` command repairs the installed GitRun systemd integration (scheduler, GSR and Control Manager units), the `gitrun-control` group and service startup, then applies the latest official signed release only if it is newer. It uses the normal updater and rollback path; it never downgrades the installed binary just to force an update. `gitrun doctor` reports configuration, dependency, installation metadata, systemd-unit, service and Control Manager socket health, with actionable failures and warnings. The command requires sudo for system installations. Use `--no-cat` to suppress the mascot.\n\nThe `fast-stop` command requires confirmation and requests force-cancellation of the **whole workflow run**, not a single job. It does not directly kill containers.

## Configuration at a glance

The example environment file is [config/config.example.env](config/config.example.env). GitRun supports GitHub App and PAT authentication.

| Setting | Purpose |
|---|---|
| `GITRUN_STATE_DIR` | Persistent scheduler and dashboard state |
| `GITRUN_LOG_DIR` | Runtime logs |
| `GITRUN_VAULT_DIR` | GitVault storage |
| `GITRUN_RUNNER_IMAGE` | Default runner image |
| `GITRUN_RUNNER_HOME_BACKEND` | Runner home storage; volume is the hardened default |
| `GITRUN_RUNNER_ROOTFS_READ_ONLY` | Read-only Linux runner root filesystem; enabled by default |
| `GITRUN_RUNNER_WINDOWS_HYPERV_ISOLATION` | Hyper-V isolation for Windows runner containers; enabled by default |
| `GITRUN_RUNNER_NETWORK` | Dedicated network for managed Linux runners |
| `GITRUN_RESOURCE_PRESSURE_PATHS` | Critical filesystem paths used for resource-pressure backpressure |
| `GITRUN_GSR_*` | GSR policy, hardening and workflow validation |
| `GITRUN_API_SOCKET` | Workflow API socket path; defaults to `/run/gitrun/api.sock` |

VM definitions are stored in `{GITRUN_STATE_DIR}/vm-configs.json`; Logic Containers rules are shared between the scheduler and dashboard.

## Security first

GitRun manages trusted infrastructure. Before connecting a repository, review who can change its workflows and trigger jobs. A workflow can use everything available inside its runner; direct Docker-socket access expands that to Docker-host authority. A VM is a useful additional boundary, but it still needs protected guest credentials, network rules and a trusted base image.

Read [Security](SECURITY.md) and the detailed [security model](docs/SECURITY_MODEL.md) before production use. GitRun's controls reduce risk; they do not make arbitrary untrusted workflows safe to run on a sensitive host.

## Documentation

| If you want to… | Start here |
|---|---|
| Understand the system and its trust boundaries | [Architecture](docs/ARCHITECTURE.md) |
| Learn the workflow APIs and their exact argument/security rules | [Git*Run API reference](docs/GITRUN_API.md) |
| Configure VM-backed runners | [VM-backed runners](docs/VM.md) |
| Understand sockets and executor authentication | [IPC and socket architecture](docs/IPC.md) |
| Configure Kitty, adaptive resources and Fast Stop | [Smart Scheduler guide](docs/SMART_SCHEDULER.md) |
| Understand security assumptions and residual risks | [Security model](docs/SECURITY_MODEL.md) and [Security policy](SECURITY.md) |
| Diagnose or report a problem | [Support](SUPPORT.md) |
| Contribute code | [Contributing](CONTRIBUTING.md) |
| Understand version-specific changes | [Release history](docs/RELEASE_HISTORY.md) |

## Development

GitRun's runtime and autoscaling control plane are Rust-native. For changes to the Rust workspace, start with:

```bash
cargo fmt --all --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Changes affecting Docker, GitHub Actions, shell/PowerShell, packaging or Tauri should also run the corresponding repository checks. CI status applies only to the commit it checks; it is not a substitute for live validation of privileged desktop flows, VM lifecycle operations or a target installation.

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) and [SUPPORT.md](SUPPORT.md).

## License

GitRun is distributed under the [Apache License 2.0](LICENSE).
