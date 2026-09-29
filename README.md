# GitRun

GitRun is a self-contained Rust control plane for Docker-based GitHub Actions self-hosted runners.

It is designed for administrators running their own repositories on a small private server. GitRun handles runner lifecycle, autoscaling, health checks, GitHub authentication, recovery, updates and host integration without Kubernetes.

## Features

- Docker-based GitHub Actions runners
- Automatic scaling with configurable warm and maximum pools
- Multiple repositories from one installation
- CPU, memory and PID limits
- CI-ready runner images with Docker CLI and PowerShell
- Persistent or ephemeral runner modes
- Automatic restart after host reboot
- Automatic container recovery when queued jobs have no online managed runner
- Shared persistent Docker cache/storage across managed runners
- Linux, macOS and Windows setup scripts
- Rust CLI for setup, configuration, diagnostics, updates, dashboard launch and repository connection
- PAT and GitHub App authentication
- Rust setup preflight for configuration, directories and host dependencies
- Self-hosted Linux x64 release deployment; Windows/macOS builds remain available through the local release scripts
- Versioned updater with checksum verification, dependency compatibility checks and rollback
- Version-pinned GHCR runner images with digest validation
- Tauri operator dashboard for configuration, health, runner pools and local controls
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

For the supported Linux server installation, GitRun uses:

- Linux x86_64 for the current prebuilt server release
- Docker Engine with Compose v2
- Git
- `sudo` for the privileged installation step when setup is run as a normal user

The GitRun Rust binary contains the manager CLI and runtime components. Runner dependencies are contained in the runner image. Managed runners receive the host Docker socket for Docker-backed CI jobs.

The repository also contains platform-specific setup and release tooling for macOS and Windows. Exact platform support depends on the relevant installer and release path.

## Installation

### Linux server

After installing GitRun, open the application normally (for example, by double-clicking its application launcher). GitRun launches the dashboard like a normal desktop application; on a first run, the graphical setup wizard guides you through the initial configuration.

If you prefer to start setup explicitly from a terminal, use:

```bash
gitrun setup
```

For a terminal-only interactive setup, use:

```bash
gitrun setup --terminal
```

The terminal wizard asks for either a GitHub Personal Access Token or GitHub App credentials, verifies repository access, then performs the privileged installation/configuration step.

The repository also contains installer scripts for platform-specific or compatibility workflows:

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
```

Use the normal graphical setup wizard for a regular first-run installation. The terminal setup is an alternative when you prefer a terminal workflow; manually creating credentials in the environment file is not required for first-run setup.

## Configuration

GitRun manages its runtime configuration during setup. `config/config.example.env` documents the supported environment variables for installations that need to manage configuration through an environment file; it is not a prerequisite for normal first-run setup.

GitRun supports two GitHub authentication modes.

### Personal Access Token

```env
GITHUB_TOKEN=
GITRUN_REPOSITORIES=owner/repository
```

### GitHub App

```env
GITRUN_GITHUB_APP_ID=
GITRUN_GITHUB_APP_INSTALLATION_ID=
GITRUN_GITHUB_APP_PRIVATE_KEY_PATH=/secure/path/to/private-key.pem
GITRUN_REPOSITORIES=owner/repository
```

The GitHub App private key is referenced by path rather than copied into the GitRun environment file. Keep that key readable only by the account that needs it.

For either authentication mode, the configured credential must have enough repository permissions to manage self-hosted runners and read Actions workflow/job state for every configured repository.

GitHub runner registration tokens are generated on demand and expire after one hour. Long-lived GitHub credentials are used by the manager to obtain the required GitHub API access.

The native dashboard supports both PAT and GitHub App authentication during setup. It can edit and persist non-secret GitRun settings; credentials are not displayed as ordinary dashboard configuration values.

### Connecting another repository

After GitRun is configured, add another repository with:

```bash
gitrun connect owner/repository
```

`gitrun connect` reuses the currently configured PAT or GitHub App authentication. It verifies access before changing the persistent configuration. Restart GitRun after connecting a repository so the scheduler reloads the repository list.

## Autoscaling

For each repository:

```text
desired = max(minimum, busy runners + queued self-hosted jobs)
desired = min(desired, maximum)
```

Extra persistent runners are removed only after the configured idle grace period.

When a self-hosted job is queued and a managed runner container is still running but its GitHub runner is offline, GitRun can restart that container automatically. If the container exists but its runner registration is missing, GitRun recreates the container. Recovery skips runners reported as busy and uses a cooldown to prevent restart loops. Disable it with `GITRUN_AUTO_CONTAINER_RECOVERY=false`.

All GitRun-managed runners use the same versioned runner image and the same persistent Docker volume for shared caches. Cargo registry/git state, Rust build targets (per repository), pip cache and npm cache survive runner replacement, so a newly created runner starts from the same toolchain and cached environment. Runner workspaces remain isolated per container. The shared volume is Docker-managed and is not removed when a runner is recreated.

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
- Dashboard service and Docker controls execute with the privileges of the account running the dashboard.

See [SECURITY.md](SECURITY.md) for deployment guidance and security reporting.

### GSR (GitSecureRun)

GSR is GitRun's built-in security layer. It has two parts:

- **Watchdog** — an external process that supervises GitRun's own processes and reports hard crashes.
- **Hardening**, in two layers (defense in depth):
  - **Internal** — every workflow `run:` step inside a runner container is evaluated against a configurable command policy (a baseline blacklist shipped with GitRun, plus your own optional blacklist and/or whitelist) *before* it executes.
  - **External** — GitRun independently polls each runner container's process list from the host and re-checks it against the same policy, as a safety net in case the internal layer is ever bypassed or missing from a custom runner image.

On a policy violation, GitRun can log the event, stop the runner container, or stop it and temporarily pause new runners for that repository — configurable from the dashboard's GSR view.

Docker-socket-mount hardening (dropped Linux capabilities, `no-new-privileges`) is applied to every runner container by default.

**Optional deeper workflow scanning.** GitRun's own workflow checks are intentionally simple. If you want broader coverage, GitRun can optionally use [zizmor](https://github.com/zizmorcore/zizmor), a third-party, MIT-licensed static analyzer for GitHub Actions workflows built by William Woodruff and the zizmor project. This is off by default, requires explicit consent to zizmor's license/terms from the dashboard before it's enabled, and — once accepted — is installed automatically via `cargo install zizmor`. See [NOTICE.md](NOTICE.md) for full attribution.

## CLI

The primary executable is `gitrun`.

### Version

```bash
gitrun -V
gitrun -v
gitrun --version
```

### Commands

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
```

`gitrun` without a command launches the dashboard.

`gitrun setup` starts the normal setup/preflight path. On a graphical installation, the normal first-run experience is the application dashboard and its setup wizard. `gitrun setup --terminal` runs the interactive terminal setup and supports both PAT and GitHub App authentication.

`gitrun config` prints the active configuration as JSON. Do not run it where its output could be exposed to untrusted users or logs.

`gitrun doctor` performs a quick configuration health check.

`gitrun update` checks for and applies GitRun updates. `gitrun rollback` restores a recorded updater backup.

## Dashboard

Launch the native dashboard with:

```bash
gitrun dashboard
```

The dashboard refreshes every five seconds and provides:

- configuration editing and persistence for non-secret GitRun settings;
- Linux systemd Start, Stop and Restart controls for the GitRun service;
- Docker Start, Stop and Restart controls for managed runner containers;
- pool, repository, health, crash and runner-container visibility.

Stopping a runner manually can be superseded by the autoscaler's next reconciliation when the configured pool requires that runner. Service controls require the dashboard process to have permission to control the configured systemd unit.

## Checks

Run the repository's static validation with:

```bash
python3 scripts/test-gitrun.py
```

For a configured host:

```bash
gitrun doctor
gitrun setup
gitrun update
gitrun dashboard
```

The updater resolves the latest GitHub release, selects the native precompiled artifact, verifies its SHA-256 checksum, preserves configuration/state, creates a rollback backup, validates the new installation with `doctor`, and updates the version-pinned runner image only when its digest is not already present. Set `GITRUN_REPOSITORY`, `GITRUN_UPDATE_DIR`, `GITRUN_INSTALL_DIR`, `GITRUN_BACKUP_DIR`, `GITRUN_CONFIG_DIR`, `GITRUN_SERVICE_CONFIG` and `GITRUN_COMPOSE_FILE` to control update locations and preserved service configuration. `gitrun rollback <backup-path>` restores a recorded backup.

Static validation does not contact GitHub and does not prove that live runners are healthy.

Tagged releases are built and published on the self-hosted Linux x64 runner with SHA-256 checksums, a machine-readable release manifest and the versioned GHCR runner image. Windows/macOS remain supported through the local release builder scripts.

## Current architecture

BigRework is migrating GitRun's manager and operator tooling into a Rust workspace. The workspace contains the Rust core, CLI, setup, updater, recovery, scheduler, vault, GSR, dashboard and Tauri dashboard components.

The Rust CLI is the primary operator entry point. The scheduler and dashboard use the shared core configuration and GitHub authentication abstractions so PAT and GitHub App behavior remains consistent across terminal and graphical setup.

GitRun's runtime control plane is Rust-native. A Rust-only Docker Compose manager profile remains available for source-based development, while the installed Linux service runs the scheduler directly through the `gitrun` executable.

See [docs/ROADMAP_PHASES.md](docs/ROADMAP_PHASES.md) and [docs/RUST_MIGRATION.md](docs/RUST_MIGRATION.md).
