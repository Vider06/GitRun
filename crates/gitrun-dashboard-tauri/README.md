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

## Data freshness and protected state

The dashboard uses cached runner, workflow, repository and host-resource snapshots so navigation does not needlessly repeat equivalent requests. Cached data can be stale or partial; unavailable values must not be presented as trustworthy zeroes. The dashboard is an operator view, not the scheduler itself, and the scheduler continues to reconcile runner state when the window is closed.

Security settings remain protected by filesystem permissions. On a system installation, the dashboard uses the fixed-purpose privileged CLI read only after a real permission-denied result; it does not make protected state user-readable or accept an arbitrary path from the UI. Internal CLI child processes explicitly pass `--no-cat` unless the caller already supplied it, keeping command output clean and avoiding conflicts with dashboard rendering. See the [v1.6.2 fixes](../../docs/v1.6.2-fixes.md) and [1.8.0 source-line notes](../../docs/v1.8.0-fixes.md).

## Structure

- `src-tauri/` — Rust/Tauri backend, native commands and startup, capability/permission declarations, packaging configuration and application icons.
- `dist/` — built frontend entry point plus its static CSS and JavaScript assets.

The `dist/` tree is a packaged frontend, not the source-of-truth for application behavior; make UI changes in the maintained frontend source and regenerate the distribution using the repository's build workflow.

The dashboard does not replace the scheduler. The scheduler remains the runtime control plane and continues operating when the graphical dashboard is closed.


The product/source target for this branch is GitRun <!-- gitrun-version:start -->1.8.4<!-- gitrun-version:end --> (`version.txt`). The release workflow sets the Tauri bundle version to the selected release version at build time; the Rust crate and frontend package retain independent version metadata. PR #83's source HEAD passed Tauri build/package and trusted privileged-binary resolution tests, but those results are historical. PR [#109](https://github.com/Vider06/GitRun/pull/109) reports its Tauri GUI gates passing; this still does not verify every desktop-specific authorization or install transition. The actual desktop `pkexec` prompt and install/uninstall/reinstall privilege transitions were not exercised by that CI. See the [1.6.2 fixes](../../docs/v1.6.2-fixes.md) and [consolidated release history](../../docs/RELEASE_HISTORY.md). The queue-aware evaluation primitives are documented in [Kitty Smart Scheduler](../../docs/SMART_SCHEDULER.md). The user-facing overview is in the [main README](../../README.md); the dashboard redesign planning document was retired after implemented behavior and remaining validation limits were folded into this crate guide.


### Kitty Future Mapping and Think

App Settings exposes opt-in controls, prerequisite validation and explicit confirmations for Future Mapping and Think. The Workflow Model view shows the map revision, actionability status, expiry, last patch metadata, twelve five-minute demand buckets and per-runner CPU/RAM envelopes, along with Think's window-agreement state. These are estimates and ceilings, not evidence that capacity was created. The scheduler's GSR audit, hard bounds, resource-pressure checks and actual container state remain authoritative.
