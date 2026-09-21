# GitRun

GitRun is a lightweight control plane for Docker-based GitHub Actions self-hosted runners.

It is designed for administrators running their own repositories on a small private server. GitRun handles runner lifecycle, autoscaling, health checks and host integration without Kubernetes.

## Features

- Docker-based GitHub Actions runners
- Automatic scaling with configurable warm and maximum pools
- Multiple repositories from one installation
- CPU, memory and PID limits
- Persistent or ephemeral runner modes
- Automatic restart after host reboot
- Linux, macOS and Windows setup scripts
- CLI for status, health and service management
- Rust setup preflight for configuration, directories and host dependencies
- Cross-platform Rust release artifacts for Linux, Windows and macOS
- Versioned updater with checksum verification, dependency compatibility checks and rollback
- Version-pinned GHCR runner images with digest validation
- No Kubernetes required

## Default profile

The default configuration is intended for a small 8 GB server:

- 3 warm runners per configured repository
- 8 runners maximum per configured repository
- 120-second idle grace period
- 5-second GitHub polling interval
- 1 CPU and 1 GB memory per runner

Multiple repositories can share one installation. Each repository currently has its own runner pool.

## Requirements

The current release uses:

- Docker Engine with Compose v2 on Linux
- Docker Desktop on macOS and Windows
- Git
- Python 3 for the local installer helpers

The manager and runner dependencies are contained in their Docker images. GitRun does not require Node.js, Python packages or Rust on the host beyond what the installer itself needs.

## Installation

### Linux

```bash
./scripts/install-linux.sh
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

The platform installers check for the required host tools, install missing system dependencies when supported, clone or update GitRun, create the local configuration and start the manager.

For a permanent Linux server installation:

```bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
sudo nano /etc/gitrun/gitrun.env
sudo systemctl start gitrun
```

## Configuration

Start from `config/config.example.env`:

```env
GITHUB_TOKEN=
GITRUN_REPOSITORIES=owner/repository

GITRUN_MIN_RUNNERS=3
GITRUN_MAX_RUNNERS=8
GITRUN_IDLE_TIMEOUT=120
GITRUN_POLL_INTERVAL=5
```

The GitHub token must have enough repository permissions to manage self-hosted runners and read Actions workflow/job state for every configured repository.

GitHub runner registration tokens are generated on demand and expire after one hour. The long-lived GitHub token is used only by the manager.

## Autoscaling

For each repository:

```text
desired = max(minimum, busy runners + queued self-hosted jobs)
desired = min(desired, maximum)
```

Extra persistent runners are removed only after the configured idle grace period.

Ephemeral mode is available when runners should be replaced after each job:

```env
GITRUN_EPHEMERAL=true
```

## Networking

Runners establish outbound HTTPS connections to GitHub. GitRun does not require inbound router port forwarding for normal operation.

Do not expose the Docker daemon or the GitRun host directly to the public Internet.

## Resource limits

Runner containers have configurable CPU, memory and PID limits. The default profile allows up to 8 GB of theoretical runner memory per repository, so the defaults should be adjusted when several repositories share a small host.

Actual host usage also includes Docker, the manager, the operating system and other services.

## Security

- Never commit `GITHUB_TOKEN` or other credentials.
- Keep runtime environment files private.
- Treat access to the Docker socket as host-level administrative access.
- Prefer ephemeral runners for workloads that should not persist between jobs.
- Review repository permissions before connecting a repository.
- Keep GitRun and runner images updated.

See [SECURITY.md](SECURITY.md) for deployment guidance and security reporting.

## CLI

```text
gitrun overview
gitrun status
gitrun repositories
gitrun runners
gitrun jobs
gitrun health
gitrun doctor
gitrun logs
gitrun last-crash
gitrun usage
gitrun config
gitrun connect owner/repository
gitrun service status
gitrun start
gitrun stop
gitrun restart
```

## Checks

Run the repository's static validation with:

```bash
python3 scripts/test-gitrun.py
```

For a configured host:

```bash
gitrun doctor
gitrun health
gitrun-rs setup
gitrun-rs update
```

The updater resolves the latest GitHub release, selects the native precompiled artifact, verifies its SHA-256 checksum, preserves configuration/state, creates a rollback backup, validates the new installation with `doctor`, and updates the version-pinned runner image only when its digest is not already present. If the runner or Docker refresh fails, the previous installation and preserved state/configuration are restored. Set `GITRUN_REPOSITORY`, `GITRUN_UPDATE_DIR`, `GITRUN_INSTALL_DIR`, `GITRUN_BACKUP_DIR` and `GITRUN_COMPOSE_FILE` to control update locations. `gitrun-rs rollback <backup.json>` restores a recorded backup.

Static validation does not contact GitHub and does not prove that live runners are healthy.

Release builds accept an optional Rust target and version. Tagged releases build Linux x64/ARM64, Windows x64 and macOS x64/ARM64 artifacts with SHA-256 checksums and a machine-readable release manifest.

## Current architecture

GitRun is transitioning from its original Python/Docker control plane toward a Rust workspace. The Phase 1–6 branch adds:

- Rust core for typed configuration, runner state, health and crash state.
- Rust CLI foundation for configuration and runner-pool operations.
- Updater and recovery primitives with explicit staging semantics.
- Cross-platform release build entry points.
- A native egui dashboard shell.
- Workspace-wide Rust formatting, clippy and test gates in CI.

The existing Python/Docker manager remains the deployment-compatible path during migration. The Rust components are intentionally additive; the manager API and runner lifecycle are migrated only after each replacement is independently verified.

See [docs/ROADMAP_PHASES.md](docs/ROADMAP_PHASES.md) and [docs/RUST_MIGRATION.md](docs/RUST_MIGRATION.md).

