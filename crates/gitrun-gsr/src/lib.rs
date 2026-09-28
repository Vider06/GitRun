//! GSR (GitSecureRun): GitRun's security layer, in two parts per the
//! operator's explicit scope correction mid-project — this is not just a
//! crash/error handler:
//!
//! 1. **Watchdog** (`watchdog.rs`): an external, separate process that
//!    supervises GitRun's other processes (starting with
//!    `gitrun-autoscaler`) via PID liveness checks, independent of
//!    them — so it can observe and report a hard crash (segfault, OOM-kill,
//!    `SIGKILL`) that a thread inside the crashed process could never see
//!    happen to itself. Shows an error graphically when possible, falls
//!    back to a terminal message, and always writes to a durable event log
//!    (`events.rs`).
//! 2. **Hardening**, in two layers agreed with the operator (defense in
//!    depth: internal enforcement as the primary control, external
//!    polling as the safety net if the internal layer is bypassed):
//!    - **Internal** (`agent.rs`, this crate's `gitrun-gsr-agent` binary):
//!      installed as the shell inside runner containers, evaluating every
//!      `run:` step's command line against `gitrun_core::command_policy`
//!      *before* it executes, refusing to run anything denied.
//!    - **External** (`gitrun-scheduler::gsr_poll`, since it needs
//!      `gitrun-scheduler`'s Docker host access): polls `docker top` on
//!      running containers from the host and re-evaluates what it sees
//!      against the same policy, catching a command that reached a
//!      container whose internal agent was bypassed or removed. See that
//!      module for `ViolationAction` handling (kill/log/kill_and_ban).
//!    Docker-socket-mount hardening (`--cap-drop`, `no-new-privileges`)
//!    lives in `gitrun-scheduler::docker` since it's a `docker run` flag
//!    decision, not something GSR itself applies at runtime.
//!
//! `events.rs` is the integration point other GitRun modules use to report
//! into GSR without depending on it — see that module's docs, and
//! `gitrun-vault`'s `VaultEventSink` trait for the first concrete producer.

pub mod agent;
pub mod events;
pub mod watchdog;

pub use events::{SecurityEvent, Severity};
pub use watchdog::{run as run_watchdog, WatchConfig};
