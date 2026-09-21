# GitRun Phases 1–6

## Phase 1 — Rust Core
Typed configuration, runner state, desired-count logic, persistent health/crash state, and a Rust CLI foundation.

## Phase 2 — Setup and dependency layer
A real Rust setup/preflight layer is now wired into the CLI. It validates the shared configuration, consumes an installed `gitrun.env` without mutating the process environment, creates the configured config/state/log directories, and checks Docker CLI, Docker daemon, Docker Compose v2 and Git availability. Linux, macOS and Windows installers now perform daemon/Compose readiness checks, create protected runtime configuration, and avoid putting the GitHub token in command-line arguments. The server installer validates Docker/Compose and validates its generated Compose configuration before enabling systemd. The Python/Docker manager remains the deployment-compatible runtime path.

## Phase 3 — Cross-platform release
Cross-platform Rust release packaging is implemented for Linux x64/ARM64, Windows x64, and macOS x64/ARM64. The release workflow builds the native `gitrun-rs` executable for each target, packages the executable with the license/readme/example configuration, publishes SHA-256 checksums and an aggregate release manifest, and can run from version tags or manual dispatch. Local Bash/PowerShell release builders produce target-specific archives and checksums, while workspace verification checks Rust formatting/checks/tests plus release metadata and packaging scripts.

## Phase 4 — Updater and recovery
Versioned manifests, safe pending-update staging, startup health state, and crash recording.

## Phase 5 — Dashboard
Native Rust/egui read-only dashboard shell for configuration and pool state.

## Phase 6 — Integration and finalization
Workspace-wide formatting/check/test gates and an additive migration contract so the current Python/Docker deployment is not broken.
