# GitRun

[![CI](https://github.com/Vider06/GitRun/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Vider06/GitRun/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Vider06/GitRun)](https://github.com/Vider06/GitRun/releases)
[![License](https://img.shields.io/github/license/Vider06/GitRun)](LICENSE)

**Your self-hosted GitHub Actions runners, under your control.**

*Born from people's problems, built to solve them.*

GitRun is a Rust-native control plane for self-hosted GitHub Actions runners. It provisions and scales Docker runners, can route selected workloads to virtual machines, and brings secrets, security policy, recovery and day-to-day operations into one desktop dashboard and CLI.

It is designed for developers and teams who want to use their own runner infrastructure without turning every workflow into an unrestricted host administrator. **No Kubernetes cluster is required.**

> **Source version:** this branch targets <!-- gitrun-version:start -->1.8.21<!-- gitrun-version:end -->. A source version or passing CI does not mean a signed release artifact is available. For installable releases, use [GitHub Releases](https://github.com/Vider06/GitRun/releases).

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
- **Signed updates:** Ed25519 release-manifest verification, streamed SHA-256 artifact verification, bounded archive extraction, durable recovery journals written before live mutation, symlink/path validation, health verification and rollback on failed installation. Full system updates share an exclusive lock with scheduled GTUU reconciliation and stop the scheduler before handing the lock to the target-version runner refresh.

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
- Setup diagnostics, path-specific JSON integrity checks, guarded repair, local-backup restoration and signed Main Library restoration.
- A host CPU utilization trend from real Linux samples (latest 60 samples) in the dashboard, with a TUI sparkline when history is available.
- Configuration, update and lifecycle controls.

The CLI supports setup, repository connection, diagnostics, update checks, dashboard launch, rollback and terminal utilities. `gitrun manage` opens a keyboard-driven terminal management console for headless servers; it reuses the dashboard's validated backend operations instead of maintaining a second settings implementation. The console uses restrained dark panel surfaces, clear selected-row highlighting and a Settings glyph chosen to avoid emoji-dependent rendering. On wide, tall terminals, a compact cat panel sits below the sections and reflects the active area and recent operation outcome; on smaller SSH terminals, the panel is omitted so navigation keeps its space. In the **Logs** section, `c` tries the native clipboard path with a bounded timeout and reports its result. Run `bash scripts/gitrun-manage.sh --target user@host` from Linux or `powershell.exe -ExecutionPolicy Bypass -File .\scripts\gitrun-manage.ps1 -Target user@host` from Windows to enable an acknowledged local clipboard copy and a Save As fallback that opens the saved file. In a plain SSH session, OSC 52 remains best effort and GitRun prepares a private `.txt` fallback with commands for both platforms. The CLI mascot is shown by default for normal commands and can be suppressed explicitly with `--no-cat`; internal child-process launches suppress it to keep terminal output clean. The scheduler continues to run when the dashboard is closed. The dashboard does not treat inaccessible protected files as proof of corruption and does not relax file permissions as a workaround.

</details>

## CLI command reference

The installed `gitrun` CLI provides these operator commands:

| Command | Purpose |
| --- | --- |
| `gitrun uninstall` | Remove GitRun services, runtime resources, package registration and application data. |
| `gitrun reinstall` | Download, verify and reinstall the latest official release. |
| `gitrun status --runner <ID>` | Inspect a GitHub Actions runner and its GitRun container. |
| `gitrun desired <min> <max> <busy> <queued>` | Calculate a desired runner count for the supplied inputs. |
| `gitrun settings` | Print the current security/API settings as JSON. |
| `gitrun api-list` | List the supported workflow-facing GitRun APIs. |
| `gitrun api-policy GitDockRun --operation mount` | Inspect the GitDockRun mount operation in the API policy contract. Use `--operation unmount` for unmount. |
| `gitrun check-compatibility [workflow-path]` | Analyze workflow files for compatibility and GitDockRun requirements; defaults to `.github/workflows`. |

`uninstall` is destructive. Review the command's confirmation prompts and ensure any required configuration or state is backed up before proceeding.

## Get started

### Requirements

For the standard Linux host deployment, prepare a dedicated Linux machine with Docker and a GitHub credential authorized for the repositories and runner operations you intend to manage. Keep the host and Docker API private. VM-backed workflows additionally require a supported hypervisor, operator-provided guest images and a configured guest Docker endpoint.

### Install on Debian or Ubuntu (recommended)

A prebuilt **AMD64/x86-64 .deb package** is published with each stable release. You do not need to clone the repository or compile Rust to install GitRun.

Download and install the current published package (this example uses v1.8.21; check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for the newest version):

```bash
VERSION="v1.8.21"
ASSET="GitRun-${VERSION}-linux-x86_64-deb.deb"
curl -fL --proto '=https' --tlsv1.2 \
  "https://github.com/Vider06/GitRun/releases/download/${VERSION}/${ASSET}" \
  -o "${ASSET}"
sudo apt install "./${ASSET}"
```

APT resolves the package's declared dependencies. Install and configure Docker separately if it is not already available on the host. The package is for Debian/Ubuntu-compatible **amd64** systems; it is not an ARM64 package.

After installation, you can complete first-run setup and configure GitRun from either the **desktop GUI** or the terminal. The GUI is a full management surface, not just a viewer; use it for setup and day-to-day configuration if you prefer a graphical workflow. From a terminal, run `gitrun setup --terminal` for the guided setup, then `gitrun manage` to open the keyboard-driven TUI for ongoing operations. The GUI and TUI share GitRun's validated backend operations.

Verify the installation with `gitrun doctor`.

### Build/install from source

Use the source installer only when you intentionally want to build the current checkout yourself:

```bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
```

Install scripts and platform-specific constraints are documented in [scripts/README.md](scripts/README.md). For published binaries and release checksums, use [GitHub Releases](https://github.com/Vider06/GitRun/releases).

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
gitrun update --force
gitrun fix-update
gitrun fast-stop OWNER/REPOSITORY WORKFLOW_RUN_ID --yes
gitrun rollback BACKUP_PATH
gitrun cat
```

The `gtuu` command inspects or reconciles GitRun-managed permanent runners. Use `gitrun gtuu --check --all` for a read-only inventory, `gitrun gtuu --rebuild --repo OWNER/REPOSITORY` to force a validated local runner-image rebuild for one configured repository, or `gitrun gtuu --all --force` to replace eligible permanent runners even when image IDs match. For system installations, launch GTUU without a `sudo` prefix: GitRun requests administrator authorization when needed, while an already elevated shell does not need another prompt. Busy runners remain untouched, and an old runner is kept until its replacement is online. Registry-qualified images stay on the signed-manifest path.

The `fix-update` command repairs the installed GitRun systemd integration (scheduler, GSR and Control Manager units), the `gitrun-control` group and service startup, then applies the latest official signed release only if it is newer. It never downgrades the installed binary. System updates verify the artifact against the signed manifest checksum, reject unsafe archive entries and symlinked destinations, and write a durable rollback journal before replacing installed files. The updater checks the executable independently before repairing services, then runs the full `doctor` check after systemd reconciliation; a failed health check normally triggers rollback and an attempt to restart the previous scheduler. `gitrun update --force` is an explicit escape hatch that skips only this post-update doctor gate. Signed-manifest verification, artifact SHA-256 validation, executable validation, systemd reconciliation and scheduler restart checks remain enabled, and failures in those stages can still fail or roll back the update. Use it only when a known doctor false negative blocks an otherwise validated update. It does not force a downgrade. The generic updater also writes its recovery journal before moving the previous installation. `gitrun doctor` reports configuration, dependency, installation metadata, systemd-unit, service and Control Manager socket health; it also checks discovered JSON files individually and distinguishes malformed content from inaccessible paths and safety-limit failures. Recovery offers a conservative repair, an explicitly confirmed restore from local `.json.Back` copies, and a signed-manifest-verified Main Library restore. Routine setup/update only creates missing backups and fills missing reference-library files rather than overwriting existing ones. System repair requires sudo. Use `--no-cat` to suppress the mascot.

The `fast-stop` command requires confirmation and requests force-cancellation of the **whole workflow run**, not a single job. It does not directly kill containers.

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
| Repair JSON safely or restore the Main Library | [CLI recovery guide](crates/gitrun-cli/README.md#json-integrity-and-recovery) and [Recovery component](crates/gitrun-recovery/README.md) |
| Understand dashboard CPU history and freshness | [Dashboard guide](crates/gitrun-dashboard-tauri/README.md#host-cpu-history) |
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
