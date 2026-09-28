# GitRun Phases 1–6

## Phase 1 — Rust Core
Typed configuration, runner state, desired-count logic, persistent health/crash state, and a Rust CLI foundation.

## Phase 2 — Setup and dependency layer
A real Rust setup/preflight layer is now wired into the CLI. It validates the shared configuration, consumes an installed `gitrun.env` without mutating the process environment, creates the configured config/state/log directories, and checks Docker CLI, Docker daemon, Docker Compose v2 and Git availability. Linux, macOS and Windows installers now perform daemon/Compose readiness checks, create protected runtime configuration, and avoid putting the GitHub token in command-line arguments. The server installer validates Docker/Compose and validates its generated Compose configuration before enabling systemd. The Python/Docker manager remains the deployment-compatible runtime path.

## Phase 3 — Release packaging and deployment
The version deploy workflow now runs entirely on the self-hosted KiloCoprServer runner, so release deployment no longer depends on GitHub-hosted Actions minutes. It builds the native Linux x64 `gitrun-rs` executable and dashboard, packages them with the license/readme/example configuration, publishes SHA-256 checksums and a machine-readable release manifest, and pushes the versioned GHCR runner image. Local Bash/PowerShell release builders remain available for target-specific Windows/macOS builds, while workspace verification checks Rust formatting/checks/tests plus release metadata and packaging scripts.

## Phase 4 — Updater and recovery
A real release updater is now implemented around the Phase 3 precompiled artifacts. It resolves the latest GitHub Release with a `version.txt` fallback, selects the native OS/architecture artifact, rejects equal/older versions, stages updates atomically, downloads and verifies SHA-256 checksums, preserves configuration/state, creates versioned backups, performs a post-install health check, and exposes explicit rollback/recovery. Dependency requirements for Git, Docker, Node.js and Python are inspected with installed-version/compatibility decisions so compatible components are skipped. Runner updates use a versioned GHCR image and pinned digest; the Docker stack refresh path uses `docker compose up --no-build` so the updater never clones or rebuilds the GitRun release locally. Failed runner/Docker refreshes restore the previous GitRun installation and preserved state/config.

## Phase 5 — Dashboard
The native Rust/egui dashboard is now an operator UI rather than read-only telemetry. It loads the effective GitRun configuration, persisted health/crash state and managed Docker runner containers; shows global pool metrics, per-repository runner counts and target pools, health/recovery information, and a runner-container table with state/image/uptime. It refreshes automatically every five seconds and provides persistent configuration editing, GitRun systemd start/stop/restart controls on Linux, and per-runner Docker start/stop/restart controls. Secret and unknown environment keys are preserved and are never rendered in the UI.

## Phase 6 — Integration and finalization

The Rust scheduler is now the authoritative runtime. The installed Linux service launches
the scheduler through the main `gitrun` executable, with no Python manager or external
GTUU process.

## Scheduled runner maintenance — GTUU
GTUU is implemented inside the Rust scheduler. It can run automatically at the configured
daily time or manually with `gitrun update --only-containers`. Permanent runners are
updated one at a time and busy runners are skipped; dynamic runners use the current image
when the autoscaler recreates them.
