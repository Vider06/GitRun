# GitRun

GitRun is a lightweight control plane for Docker-based GitHub Actions self-hosted runners.

## Features

- Docker-based runner management
- Repository-level autoscaling
- Configurable warm pool and idle timeout
- Multi-repository support
- CPU, memory and PID limits
- Automatic recovery after host or container restart
- Linux, macOS and Windows installers
- Small CLI for status, health and lifecycle management
- No Kubernetes required

## Default profile

The default configuration targets a small 8 GB server:

- 3 warm runners per repository
- 8 runners maximum per repository
- 120-second idle grace period
- 5-second GitHub polling interval
- 1 CPU and 1 GB memory per runner

Repository-level runners are dedicated to their repository. Multiple repositories can share one GitRun installation, with a separate pool for each repository.

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

The installers set up the local Docker environment, clone or update GitRun, create the configuration, and start the manager. Runner registration is handled automatically.

For a permanent server installation:

```bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
sudo nano /etc/gitrun/gitrun.env
sudo systemctl start gitrun
```

## Configuration

Copy `config/config.example.env` to your local configuration file and set:

```env
GITHUB_TOKEN=
GITRUN_REPOSITORIES=owner/repository

GITRUN_MIN_RUNNERS=3
GITRUN_MAX_RUNNERS=8
GITRUN_IDLE_TIMEOUT=120
GITRUN_POLL_INTERVAL=5
```

The GitHub token must have sufficient permissions to manage self-hosted runners and read Actions workflow/job state for every configured repository.

Registration tokens are generated on demand and expire after one hour.

## Autoscaling

For each configured repository, GitRun calculates:

```text
desired = max(minimum, busy runners + queued self-hosted jobs)
desired = min(desired, maximum)
```

Extra persistent runners are removed only after they have remained idle for the configured grace period.

Ephemeral mode is also available:

```env
GITRUN_EPHEMERAL=true
```

Ephemeral runners are useful when stronger job isolation is required.

## Networking

GitHub Actions runners normally establish outbound HTTPS connections to GitHub. GitRun does not require inbound router port forwarding.

Do not expose the Docker daemon or GitRun's host environment directly to the Internet.

## Resource limits

Runner containers are constrained with Docker CPU, memory and PID limits. These are configurable through the environment file.

The default 8-runner profile allows up to 8 GB of theoretical runner memory across containers. Actual host usage also includes Docker, the manager, the operating system and other services.

## Security

- Never commit `GITHUB_TOKEN`.
- Keep the runtime environment file private and mode `0600` on Linux.
- Treat access to the Docker socket as host-level administrative access.
- Prefer ephemeral runners for untrusted workloads.
- Do not expose Docker or runner management interfaces to the public Internet.
- Review repository runner permissions before connecting additional repositories.

## CLI

```text
gitrun overview
gitrun status
gitrun repositories
gitrun runners
gitrun health
gitrun doctor
gitrun logs
gitrun last-crash
gitrun usage
gitrun connect owner/repository
gitrun service status
gitrun start
gitrun stop
gitrun restart
```

## Checks

Run the static validation with:

```bash
python3 scripts/test-gitrun.py
```

For a configured host, use:

```bash
gitrun doctor
gitrun health
```

The static check does not contact GitHub and does not prove that live runners are healthy.

## Roadmap

- GitHub App authentication
- Per-repository pool settings
- Rich terminal dashboard
- Runner history and diagnostics
- Image update command
- Per-repository labels and runner groups
- Just-in-time runner support
- Metrics
- Optional web dashboard

GitRun is intentionally designed as a small host control plane for single-server Docker deployments rather than a Kubernetes-based runner platform.
