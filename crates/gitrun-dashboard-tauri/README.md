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
- `src-tauri/` — native commands/startup, capability and permission declarations, packaging configuration and application icons.
- `dist/` — built frontend entry point plus its static CSS and JavaScript assets.

The `dist/` tree is a packaged frontend, not the source-of-truth for application behavior; make UI changes in the maintained frontend source and regenerate the distribution using the repository's build workflow.

The dashboard does not replace the scheduler. The scheduler remains the runtime control plane and continues operating when the graphical dashboard is closed.


The product/bundle metadata is aligned to GitRun 1.6.2 in this source line; the Rust crate retains its own independent Cargo SemVer. PR #83's source HEAD passed Tauri build/package and trusted privileged-binary resolution tests, but those results are historical. PR [#109](https://github.com/Vider06/GitRun/pull/109) reports its Tauri GUI gates passing; this still does not verify every desktop-specific authorization or install transition. The actual desktop `pkexec` prompt and install/uninstall/reinstall privilege transitions were not exercised by that CI. See the [1.6.2 fixes](../../docs/v1.6.2-fixes.md), [1.5.1 recovery notes](../../docs/v1.5.1-release-notes.md), and [historical status report](../../docs/v1.5.0-refactor-status.md).
