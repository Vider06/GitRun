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
    fn on_secret_read(&self, secret_name: &str, scope: &gitrun_vault::Scope) {
        let event = SecurityEvent::new(
            "gitvault",
            Severity::Info,
            format!("secret read: {secret_name} scope={scope:?}"),
        );
        let _ = gitrun_gsr::events::emit(&self.events_path, &event);
    }

    fn on_secret_write(&self, secret_name: &str, scope: &gitrun_vault::Scope) {
        let event = SecurityEvent::new(
            "gitvault",
            Severity::Info,
            format!("secret write: {secret_name} scope={scope:?}"),
        );
        let _ = gitrun_gsr::events::emit(&self.events_path, &event);
    }

    fn on_secret_delete(&self, secret_name: &str, scope: &gitrun_vault::Scope) {
        let event = SecurityEvent::new(
            "gitvault",
            Severity::Warning,
            format!("secret delete: {secret_name} scope={scope:?}"),
        );
        let _ = gitrun_gsr::events::emit(&self.events_path, &event);
    }

    fn on_randomness_failure(&self, operation: &str) {
        let event = SecurityEvent::new(
            "gitvault",
            Severity::Critical,
            format!("cryptographic randomness unavailable while {operation}"),
        );
        if let Err(error) = gitrun_gsr::events::emit(&self.events_path, &event) {
            eprintln!(
                "gitrun-autoscaler: failed to emit GitVault randomness-failure event to {}: {error}",
                self.events_path.display()
            );
        }
    }

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
        if let Err(error) = gitrun_gsr::events::emit(&self.events_path, &event) {
            eprintln!(
                "gitrun-autoscaler: failed to emit GitVault decryption-failure event to {}: {error}",
                self.events_path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gitrun-gsr-bridge-test-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn randomness_failure_is_emitted_as_critical_gsr_event() {
        let state_dir = temp_state_dir();
        let state_dir_str = state_dir.to_string_lossy().into_owned();
        let bridge = VaultToGsrBridge::new(&state_dir_str);

        bridge.on_randomness_failure("generating the vault master key");

        let events_path = gitrun_gsr::events::default_queue_path(&state_dir_str);
        let events = gitrun_gsr::events::read_all(&events_path).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "gitvault");
        assert_eq!(events[0].severity, Severity::Critical);
        assert!(events[0]
            .message
            .contains("cryptographic randomness unavailable"));
        assert!(events[0].message.contains("vault master key"));

        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn decryption_failure_is_emitted_to_gsr_queue() {
        let state_dir = temp_state_dir();
        let state_dir_str = state_dir.to_string_lossy().into_owned();
        let bridge = VaultToGsrBridge::new(&state_dir_str);

        bridge.on_decryption_failure("API_KEY");

        let events_path = gitrun_gsr::events::default_queue_path(&state_dir_str);
        let events = gitrun_gsr::events::read_all(&events_path).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "gitvault");
        assert_eq!(events[0].severity, Severity::Critical);
        assert!(events[0].message.contains("API_KEY"));

        let _ = std::fs::remove_dir_all(state_dir);
    }
}
