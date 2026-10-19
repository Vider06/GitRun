# GitRun Tauri dashboard

This crate contains the native Tauri 2 operator dashboard.

## Responsibilities

The dashboard is the operator-facing control surface for:

- GitRun configuration and repository settings;
- runner pool and health visibility;
- GSR command-policy and hardening settings;
- GitVault secret management;
- Logic Containers rules;
- VM definitions and hypervisor decisions;
- service/container lifecycle controls;
- recovery and state diagnostics.

The Tauri backend shares persistent VM and Logic Containers formats with the scheduler. In particular, VM definitions are stored in `{GITRUN_STATE_DIR}/vm-configs.json`.

## Structure

- `src-tauri/` — Rust/Tauri application backend.
- `src-tauri/src/` — native commands and startup.
- `src-tauri/capabilities/` — Tauri capability declarations.
- `src-tauri/permissions/` — custom permissions.
- `src-tauri/icons/` — application icons.
- `dist/` — static dashboard frontend distribution.

The dashboard does not replace the scheduler. The scheduler remains the runtime control plane and continues operating when the graphical dashboard is closed.


The v1.5.0 CI suite builds/packages the dashboard and tests trusted privileged-binary resolution, including symlink rejection. A successful build/test does not exercise the actual desktop `pkexec` prompt or every install/uninstall/reinstall privilege transition; those remain separate environment-level verification items in the [status tracker](../../docs/v1.5.0-refactor-status.md).
