# Rust runtime architecture

GitRun's runtime and autoscaling control plane is Rust-native. The `gitrun-scheduler`
crate owns reconciliation, GitHub API access, runner lifecycle, recovery and GTUU.
The installed Linux service runs the same scheduler runtime through the main
`gitrun` executable, so there is no Python manager or second runtime to keep in sync.

The old Python autoscaler and external GTUU are retired. The current Rust runtime
provides GitHub pagination, rate-limit handling, persistent scheduler state, runner
recovery and the Rust GTUU implementation.

The operator dashboard is now the Tauri 2 application in
`crates/gitrun-dashboard-tauri`. The CLI launches that application for `gitrun dashboard`
and when no command is supplied; the retired egui dashboard is no longer a workspace
member or release artifact.

A Rust-only Docker Compose manager profile remains available for source-based
development/compatibility workflows; it is not required by the installed Linux service.
`systemd/gitrun.service` launches `/usr/local/bin/gitrun scheduler` directly.


## Post-migration runtime additions

The Rust runtime now also owns the Git*Run security/execution path: `gitrun-core` defines the closed API policy, `gitrun-gsr` authorizes workflow requests, and `gitrun-exe` is the internal execution boundary. Workflow requests use the local Unix API socket documented in [GITRUN_API.md](GITRUN_API.md) and [IPC.md](IPC.md).

VM-backed Logic Containers are part of the scheduler runtime. VM definitions are shared with the Tauri dashboard through `{GITRUN_STATE_DIR}/vm-configs.json`; KVM/libvirt is preferred and VirtualBox is the fallback path when KVM is unavailable. See [VM.md](VM.md).
