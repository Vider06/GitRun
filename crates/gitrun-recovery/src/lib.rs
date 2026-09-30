pub mod ui;

use gitrun_core::StateStore;
use gitrun_updater::{rollback, BackupRecord, UpdateError, UpdatePaths};

pub fn startup_recovery(store: &StateStore) -> Result<(), Box<dyn std::error::Error>> {
    store.write_health(true, "recovery subsystem initialized")?;
    Ok(())
}

pub fn record_failure(store: &StateStore, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    store.write_health(false, message)?;
    store.record_crash(message)?;
    Ok(())
}

pub fn restore_update(paths: &UpdatePaths, backup: &BackupRecord) -> Result<(), UpdateError> {
    rollback(paths, backup)
}
