//! Concrete bridge from GitVault's `VaultEventSink` trait (defined in
//! `gitrun-vault`, which does not depend on `gitrun-gsr` at all) to GSR's
//! event queue (`gitrun-gsr::events`). This is the "gate" the operator
//! asked to keep open between modules from early in the session: GitVault
//! can report a tamper/decryption-failure event without ever knowing GSR
//! exists, and this small piece of glue — living in `gitrun-scheduler`,
//! which already depends on both — is what actually connects them.
//!
//! Living here rather than in `gitrun-vault` or `gitrun-gsr` themselves
//! keeps both of those crates' dependency graphs clean: `gitrun-vault` only
//! knows about its own trait, `gitrun-gsr` only knows about its own event
//! queue, and this crate is the one place that's already allowed to know
//! about both.

use gitrun_gsr::{SecurityEvent, Severity};
use gitrun_vault::VaultEventSink;
use std::path::PathBuf;

pub struct VaultToGsrBridge {
    events_path: PathBuf,
}

impl VaultToGsrBridge {
    pub fn new(state_dir: &str) -> Self {
        Self {
            events_path: gitrun_gsr::events::default_queue_path(state_dir),
        }
    }
}

impl VaultEventSink for VaultToGsrBridge {
    fn on_decryption_failure(&self, secret_name: &str) {
        let event = SecurityEvent::new(
            "gitvault",
            Severity::Critical,
            format!("decryption failed for secret '{secret_name}' — wrong master key or tampered ciphertext"),
        );
        // Best-effort: if the event log itself can't be written, there's
        // nowhere else for this bridge to escalate to. The caller
        // (`vault_env_for_repo` in main.rs) still logs the underlying error
        // to stderr independently, so the operator isn't left with zero
        // signal even if this write fails.
        let _ = gitrun_gsr::events::emit(&self.events_path, &event);
    }
}
