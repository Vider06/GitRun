# GitRun Phases 1–6

## Phase 1 — Rust Core
Typed configuration, runner state, desired-count logic, persistent health/crash state, and a Rust CLI foundation.

## Phase 2 — Setup and dependency layer
A real Rust setup/preflight layer is now wired into the CLI. It validates the shared configuration, consumes an installed `gitrun.env` without mutating the process environment, creates the configured config/state/log directories, and checks Docker CLI, Docker daemon, Docker Compose v2 and Git availability. Linux, macOS and Windows installers now perform daemon/Compose readiness checks, create protected runtime configuration, and avoid putting the GitHub token in command-line arguments. The server installer validates Docker/Compose and validates its generated Compose configuration before enabling systemd. The Python/Docker manager remains the deployment-compatible runtime path.

## Phase 3 — Cross-platform release
Cross-platform Rust release packaging is implemented for Linux x64/ARM64, Windows x64, and macOS x64/ARM64. The release workflow builds the native `gitrun-rs` executable for each target, packages the executable with the license/readme/example configuration, publishes SHA-256 checksums and an aggregate release manifest, and can run from version tags or manual dispatch. Local Bash/PowerShell release builders produce target-specific archives and checksums, while workspace verification checks Rust formatting/checks/tests plus release metadata and packaging scripts.

## Phase 4 — Updater and recovery
A real release updater is now implemented around the Phase 3 precompiled artifacts. It resolves the latest GitHub Release with a `version.txt` fallback, selects the native OS/architecture artifact, rejects equal/older versions, stages updates atomically, downloads and verifies SHA-256 checksums, preserves configuration/state, creates versioned backups, performs a post-install health check, and exposes explicit rollback/recovery. Dependency requirements for Git, Docker, Node.js and Python are inspected with installed-version/compatibility decisions so compatible components are skipped. Runner updates use a versioned GHCR image and pinned digest; the Docker stack refresh path uses `docker compose up --no-build` so the updater never clones or rebuilds the GitRun release locally. Failed runner/Docker refreshes restore the previous GitRun installation and preserved state/config.

## Phase 5 — Dashboard
The native Rust/egui dashboard is now an operator UI rather than read-only telemetry. It loads the effective GitRun configuration, persisted health/crash state and managed Docker runner containers; shows global pool metrics, per-repository runner counts and target pools, health/recovery information, and a runner-container table with state/image/uptime. It refreshes automatically every five seconds and provides persistent configuration editing, GitRun systemd start/stop/restart controls on Linux, and per-runner Docker start/stop/restart controls. Secret and unknown environment keys are preserved and are never rendered in the UI.

## Phase 6 — Integration and finalization
Workspace-wide formatting/check/test gates and an additive migration contract so the current Python/Docker deployment is not broken.
