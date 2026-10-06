//! GSR (GitSecureRun): GitRun's security layer, in three enforcement pieces
//! plus the independent crash watchdog:
//!
//! 1. Watchdog (watchdog.rs): a separate process that supervises
//!    gitrun-autoscaler so it can report hard crashes the watched process
//!    could never observe itself.
//! 2. Shell Layer 1 (agent.rs): the lightweight shell replacement in
//!    runner containers, evaluating GitHub Actions run commands before the
//!    real shell starts.
//! 3. Kernel Layer 1.5 (exec_supervisor.rs): a root PID-1 supervisor
//!    that traces the unprivileged Actions runner and all descendants. A
//!    seccomp filter converts execve/execveat to ptrace stops, allowing the
//!    same command policy to be checked synchronously before any executable
//!    starts. This closes the direct-execve and short-lived-process gaps that
//!    a shell wrapper plus periodic docker top polling cannot close.
//! 4. External Layer 2 (gitrun-scheduler::gsr_poll): host-side polling
//!    remains as defense in depth for containers whose kernel supervisor is
//!    absent or compromised. It can terminate the whole container regardless
//!    of what happened inside it.
//!
//! The Docker runner keeps CAP_SYS_PTRACE only for the PID-1 supervisor. The
//! supervisor permanently drops the capability set before launching the
//! Actions runner, so workflow code does not inherit that tracing capability.
//! events.rs remains the shared integration point for durable security events.

pub mod agent;
pub mod api_gate;
pub mod events;
pub mod exec_supervisor;
pub mod watchdog;

pub use events::{SecurityEvent, Severity};
pub use watchdog::{run as run_watchdog, WatchConfig};
