# GitRun Phases 1–6

## Phase 1 — Rust Core
Typed configuration, runner state, desired-count logic, persistent health/crash state, and a Rust CLI foundation.

## Phase 2 — Setup and dependency layer
New components share Rust configuration/state primitives while the existing Python manager remains compatible.

## Phase 3 — Cross-platform release
Linux and PowerShell release build entry points plus a release manifest format and workspace verification.

## Phase 4 — Updater and recovery
Versioned manifests, safe pending-update staging, startup health state, and crash recording.

## Phase 5 — Dashboard
Native Rust/egui read-only dashboard shell for configuration and pool state.

## Phase 6 — Integration and finalization
Workspace-wide formatting/check/test gates and an additive migration contract so the current Python/Docker deployment is not broken.
