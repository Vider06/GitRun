# Rust runtime architecture

GitRun's runtime and autoscaling control plane is Rust-native. The `gitrun-scheduler`
crate owns reconciliation, GitHub API access, runner lifecycle, recovery and GTUU.
The installed Linux service runs the same scheduler runtime through the main
`gitrun` executable, so there is no Python manager or second runtime to keep in sync.

The old Python autoscaler and external GTUU are retired. The current Rust runtime
provides GitHub pagination, rate-limit handling, persistent scheduler state, runner
recovery and the Rust GTUU implementation.

A Rust-only Docker Compose manager profile remains available for source-based
development/compatibility workflows; it is not required by the installed Linux service.
`systemd/gitrun.service` launches `/usr/local/bin/gitrun scheduler` directly.
