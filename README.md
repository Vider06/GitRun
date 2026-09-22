# GitRun

[![CI](https://github.com/Vider06/GitRun/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Vider06/GitRun/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Vider06/GitRun)](https://github.com/Vider06/GitRun/releases)
[![License](https://img.shields.io/github/license/Vider06/GitRun)](LICENSE)

GitRun is a lightweight control plane for Docker-based GitHub Actions self-hosted runners.

It manages runner registration, container lifecycle, autoscaling, recovery, configuration, updates, and an operator dashboard without requiring Kubernetes.

> **Security boundary:** GitRun-managed runners intentionally mount the host Docker socket so trusted CI jobs can use Docker. A workflow with access to that socket can potentially control the Docker host. Only connect repositories whose workflow code you trust. See docs/SECURITY_MODEL.md.

## What GitRun provides

- Configurable warm and maximum runner pools per repository.
- Automatic scaling from busy runners and queued self-hosted jobs.
- Permanent and dynamically-created runners.
- Automatic recovery when managed runners disappear or go offline.
- Shared Docker-managed caches for Cargo, pip, npm, and Rust build targets.
- Read-only runner image roots with a dedicated writable runner work directory.
- CPU, memory, and PID limits.
- Docker CLI, Compose, PowerShell, Rust, Python, GitHub CLI, and common CI tooling in the runner image.
- Native Rust/egui operator dashboard.
- Linux systemd deployment and a self-contained Debian package.
- Checksum-verified release updates with backup and rollback support.

## Default configuration

| Setting | Default |
| --- | ---: |
| Minimum runners | 3 |
| Maximum runners | 8 |
| Idle timeout | 120 seconds |
| GitHub poll interval | 5 seconds |
| Runner CPU limit | 1 CPU |
| Runner memory limit | 1 GB |
| Runner PID limit | 1024 |

## Requirements

For a Linux server installation:

- Docker Engine
- Docker Compose v2
- Git
- A GitHub credential with permission to manage self-hosted runners and inspect Actions state for the configured repositories

## Install

### Linux desktop

~~~bash
./scripts/install-linux.sh
~~~

### Linux server

~~~bash
sudo ./scripts/install-server.sh
sudoedit /etc/gitrun/gitrun.env
sudo systemctl start gitrun.service
gitrun doctor
~~~

### Windows and macOS

Local build/install helpers are provided under scripts/. The current GitHub release pipeline publishes the Linux x86_64 Debian package; Windows/macOS packaging can be built locally with the platform-specific release scripts.

## Configuration

~~~env
GITHUB_TOKEN=
GITRUN_REPOSITORIES=owner/repository

GITRUN_MIN_RUNNERS=3
GITRUN_MAX_RUNNERS=8
GITRUN_IDLE_TIMEOUT=120
GITRUN_POLL_INTERVAL=5

GITRUN_AUTO_CONTAINER_RECOVERY=true
GITRUN_CONTAINER_RECOVERY_COOLDOWN=60
GITRUN_SHARED_CACHE_VOLUME=gitrun-runner-shared

GITRUN_RUNNER_IMAGE=gitrun-runner:latest
GITRUN_RUNNER_LABELS=self-hosted,Linux,X64,gitrun-ci
GITRUN_EPHEMERAL=false
~~~

Never commit the runtime environment file or a real GitHub credential.

## Autoscaling

For each configured repository, GitRun calculates a desired pool size from current demand:

~~~text
desired = clamp(minimum, busy_online_runners + queued_self_hosted_jobs, maximum)
~~~

GitRun creates permanent runners until the minimum pool exists, then uses dynamic runners for additional demand. Idle runners above the configured minimum can be removed after the idle timeout.

## Docker-backed CI

Managed runners mount the host Docker socket so CI workloads can use Docker. This is intentional, but it means runner workflows must be trusted like host-administrator code.

For stronger job isolation, consider ephemeral runners and an architecture that does not expose the host Docker socket.

## CLI

~~~text
gitrun version
gitrun config
gitrun desired
gitrun doctor
gitrun setup
gitrun update
gitrun rollback <backup.json>
gitrun dashboard
~~~

## Updates and rollback

The updater resolves the configured GitHub release, verifies the downloaded artifact checksum, preserves configuration/state, creates a backup, performs post-install validation, and rolls back the installation if a required update stage fails.

~~~bash
gitrun update
~~~

## Development

~~~bash
python3 scripts/test-gitrun.py
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
~~~

CI runs the applicable gates on self-hosted Linux runners.

## Project structure

~~~text
autoscaler/        Python manager and update utility
bin/               command wrapper
config/            example runtime configuration
crates/            Rust workspace
docker/            manager and runner images
docs/              architecture and operational documentation
release/           release manifest schema/examples
scripts/           install, test, and release helpers
systemd/           Linux service unit
~~~

GitRun is in an additive Rust migration: the existing Python/Docker manager remains the deployment-compatible control plane while Rust components replace responsibilities incrementally.

See CONTRIBUTING.md, SECURITY.md, docs/SECURITY_MODEL.md, docs/RUST_MIGRATION.md, and docs/ROADMAP_PHASES.md.

## License

GitRun is distributed under the MIT License.
