# GitRun management backend

`gitrun-management` is the shared implementation for GitRun's operator-facing management operations. It owns management reads and writes, validation, configuration persistence, the typed action dispatcher, and the privileged Control Manager's Unix-domain-socket protocol and service loop.

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
