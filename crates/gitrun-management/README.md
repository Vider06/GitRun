# GitRun management backend

`gitrun-management` is the shared implementation for GitRun's operator-facing management operations. It owns management reads and writes, validation, general configuration persistence, the typed action dispatcher, and the privileged Control Manager's Unix-domain-socket protocol and service loop. `gitrun-updater` owns update policy persistence, host diagnostics, update-lock inspection and bounded operation history; Management exposes those APIs to GUI and TUI clients without duplicating updater policy or state.

## Runtime boundaries

- The default feature set is headless. It does not enable Tauri and is used by the CLI, terminal management console, and the systemd Control Manager.
- The `desktop` feature enables the optional Tauri integration used by the separately packaged Dashboard executable.
- `gitrun-dashboard-tauri` is a thin desktop facade; it does not own a second copy of the management backend or Control Manager.
- The CLI depends on `gitrun-management`, not on the Dashboard crate. The `gitrun dashboard` command launches the separately packaged `gitrun-dashboard-tauri` executable.

## Build and test

Headless backend:

```sh
cargo test --locked -p gitrun-management --no-default-features --all-targets
cargo clippy --locked -p gitrun-management --no-default-features --all-targets -- -D warnings
```

Dashboard integration:

```sh
cargo test --locked -p gitrun-management --features desktop
cargo build --locked -p gitrun-dashboard-tauri
```

The Control Manager is started by the installed GitRun CLI's internal `--control-manager` entry point under `gitrun-control.service`. It is not hosted by the Dashboard process and does not require a graphical session.

## Updater management boundary

`gitrun-management` owns the persisted Security/Full/Pre release-line preference, local updater diagnostics, bounded operation/error history, and the shared GUI/TUI/CLI management interface. The Dashboard and Management TUI are clients of this backend; neither implements release selection or maintains a separate policy store.

`gitrun-updater` remains the engine for signed manifest verification, GitHub release enumeration, SemVer policy selection and target-compatible update planning. Management calls that engine for checks and exposes the result to each interface.

Protected Linux installations use Control Manager IPC on systemd. Other protected Linux service-manager installations use the fixed-purpose privileged CLI helpers exposed by the management backend.

## Immediate operator operations and scheduled maintenance

Commands explicitly selected by an operator in the Management TUI or Dashboard run in the foreground. The TUI attaches the CLI child to its terminal, suspends the management viewport while the command runs, and returns only after the process exits. The Dashboard uses the backend's streaming privileged-command runner, forwards stdout/stderr events to the UI and awaits the real exit status.

User-requested update, permanent-runner update, reinstall and uninstall actions are not submitted as `maintenance_task` jobs. This prevents an interface from reporting that a task was merely handed to systemd when the operator expects to watch it run.

The Control Manager's `maintenance_task` mechanism remains available for work that is intentionally deferred or background-managed. It is not the default execution path for direct TUI/GUI actions. Recurring runner/container automation remains owned by the scheduler and its configured schedule.