# GitRun

GitRun is a lightweight control plane for Docker-based GitHub Actions self-hosted runners.

## Design

GitRun is designed for a single Ubuntu server and does not require Kubernetes.

Default pool:

- 3 warm runners
- 20 hard maximum
- extra runners are retained for 120 seconds after they actually become idle
- multiple private repositories can share one GitRun installation
- runner containers have CPU, memory and PID limits
- Docker restart policies provide boot/crash recovery
- systemd starts the GitRun manager after host reboot

The autoscaler polls GitHub every 5 seconds by default. It counts queued jobs that target self-hosted runners and scales the repository pool up to the configured maximum.

## CLI

~~~text
gitrun overview
gitrun status
gitrun repositories
gitrun runners
gitrun health
gitrun doctor
gitrun logs
gitrun last-crash
gitrun usage

gitrun connect Vider06/Eridania_Site

gitrun service status
gitrun service start
gitrun service stop
gitrun service restart

gitrun start
gitrun stop
gitrun restart
~~~

## Server installation

On the Ubuntu server:

~~~bash
git clone https://github.com/Vider06/GitRun.git /opt/gitrun-source
cd /opt/gitrun-source
sudo ./scripts/install-server.sh
sudo nano /etc/gitrun/gitrun.env
sudo systemctl start gitrun
gitrun doctor
gitrun overview
~~~

The installer installs the CLI, installs the GitRun manager and runner sources under /opt/gitrun, builds both images, creates /etc/gitrun/gitrun.env, enables gitrun.service, and creates persistent state/log directories.

## GitHub authentication

GitRun currently uses GITHUB_TOKEN for the GitHub REST API.

The token needs enough repository access to create runner registration tokens, list/manage repository self-hosted runners, and read Actions workflow/job state.

GitHub's current REST API supports repository registration tokens, and those registration tokens expire after one hour; GitRun requests a fresh registration token whenever it creates a runner. citeturn2search7

For a long-lived production deployment, GitHub App authentication is a planned upgrade because it gives more granular credentials.

## Runner image

The initial image is based on Node.js 22 and installs the GitHub Actions runner application. The runner version is pinned in docker/runner/Dockerfile and should be updated when GitHub releases a newer supported version.

GitHub is enforcing minimum self-hosted runner versions, so keeping the image current is part of normal GitRun maintenance. citeturn1news2turn1news5

## Autoscaling behavior

For each configured repository:

~~~text
minimum = 3
maximum = 20

queued self-hosted jobs:
  0  -> keep 3
  1  -> keep 3
  2  -> keep 3
  3  -> scale toward 3+
  ...
  17 -> scale toward 20
  18+ -> cap at 20
~~~

The exact desired count is:

~~~text
max(minimum, busy runners + queued self-hosted jobs)
~~~

capped at maximum.

When demand falls, GitRun records the moment each runner becomes idle. Extra runners are removed only after the configured 120-second grace period, and the pool never falls below the minimum.

## Persistent vs ephemeral runners

The requested warm pool uses persistent runner containers because a runner registered with --ephemeral is automatically de-registered after one job. GitHub recommends ephemeral runners for autoscaling because they provide stronger job-to-job isolation. citeturn3search0

GitRun already exposes:

~~~env
GITRUN_EPHEMERAL=true
~~~

for a future one-job-per-container mode.

For production workloads involving untrusted pull requests, secrets or multiple independent users, ephemeral runners are preferable. GitHub explicitly warns that self-hosted runners can be persistently compromised by untrusted workflow code. citeturn3search1

## Networking

No inbound router port-forward is required for normal GitHub Actions runner operation.

Self-hosted runners establish outbound HTTPS connections to GitHub over port 443. citeturn0search0

Therefore the planned home-server setup should normally be:

~~~text
GitHub
   ↑
   │ outbound HTTPS :443
   │
Router
   │
Ubuntu mini-server
   │
Docker
   ├── GitRun manager
   └── runner containers
~~~

Do not expose the Docker daemon, GitRun API, or runner ports to the Internet.

## Resource limits

Runner containers have configurable CPU, memory and PID limits:

~~~env
GITRUN_CONTAINER_CPUS=2
GITRUN_CONTAINER_MEMORY=4g
GITRUN_CONTAINER_PIDS=2048
~~~

These are hard per-container limits, not reservations. Twenty containers with a 4 GiB limit could theoretically be allowed to consume up to 80 GiB, so the maximum of 20 is a logical scaling cap, not a promise that the server can actually run 20 heavy builds concurrently. Docker recommends applying resource limits to reduce the risk of host-wide OOM conditions. citeturn5search0

The values must therefore be tuned to the actual mini-server hardware.

## Docker startup

GitRun uses Docker's restart policy for the manager and runner containers and a systemd unit to bootstrap the manager after boot. Docker documents restart policies as the preferred mechanism for automatically starting containers. citeturn0search2

## Checks

Run:

~~~bash
python3 scripts/test-gitrun.py
gitrun doctor
gitrun health
~~~

The static check does not contact GitHub and does not claim that live runners are healthy. Live health is only established after the server is configured and GitHub reports the runners as online/idle. GitHub exposes those runner states through the self-hosted runner APIs. citeturn2search0

## Billing

GitHub Actions usage is free for self-hosted runners. The GitHub Free allowance of 2,000 minutes applies to standard GitHub-hosted runners for private repositories. Changing repositories does not reset that account-level allowance. citeturn6search0

Using GitRun therefore avoids consuming those hosted-runner minutes for workflows that actually execute on the self-hosted pool.

## Security rules

- GitRun should be used with private repositories unless a deliberate trust model exists.
- Never commit GITHUB_TOKEN.
- Keep /etc/gitrun/gitrun.env mode 0600.
- Do not mount the host Docker socket into job runners unless Docker-in-Docker/host Docker access is explicitly required.
- Keep runner networks isolated from sensitive LAN services.
- Use repository runner groups/policies when organization-level runners are introduced.
- Prefer ephemeral mode for workflows that execute untrusted code.

## Roadmap

- GitHub App authentication.
- Repository selector with per-repository min/max pools.
- Rich terminal dashboard.
- Runner event/history database.
- Crash history and automatic diagnostics.
- gitrun update for runner image updates.
- Per-repository labels and runner groups.
- Optional ephemeral/JIT runner mode.
- Prometheus-compatible metrics.
- Optional web dashboard.

GitRun is intentionally being built as a small host control plane rather than adopting Kubernetes/ARC. GitHub's official autoscaling runner sets are designed around ARC/Kubernetes; GitRun targets a single Ubuntu/Docker server where that additional orchestration layer would be unnecessary overhead. citeturn0search3turn0search12
