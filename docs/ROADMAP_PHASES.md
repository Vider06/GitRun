# GitRun Phases 1–6

## Phase 1 — Rust Core
Typed configuration, runner state, desired-count logic, persistent health/crash state, and a Rust CLI foundation.

## Phase 2 — Setup and dependency layer
A real Rust setup/preflight layer is now wired into the CLI. It validates the shared configuration, consumes an installed `gitrun.env` without mutating the process environment, creates the configured config/state/log directories, and checks Docker CLI, Docker daemon, Docker Compose v2 and Git availability. Linux, macOS and Windows installers now perform daemon/Compose readiness checks, create protected runtime configuration, and avoid putting the GitHub token in command-line arguments. The server installer validates Docker/Compose and validates its generated Compose configuration before enabling systemd. The Python/Docker manager remains the deployment-compatible runtime path.

## Phase 3 — Cross-platform release
Linux and PowerShell release build entry points plus a release manifest format and workspace verification.

## Phase 4 — Updater and recovery
Versioned manifests, safe pending-update staging, startup health state, and crash recording.

## Phase 5 — Dashboard
Native Rust/egui read-only dashboard shell for configuration and pool state.

## Phase 6 — Integration and finalization
Workspace-wide formatting/check/test gates and an additive migration contract so the current Python/Docker deployment is not broken.
