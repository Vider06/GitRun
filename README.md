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
```

Static validation does not contact GitHub and does not prove that live runners are healthy.

## Current architecture

The current release is a Python/Docker control plane. A Rust core, native desktop GUI, cross-platform packaging and an integrated update/recovery system are planned as the next major development phase.

The project is intentionally kept small and host-oriented rather than built as a Kubernetes platform.
