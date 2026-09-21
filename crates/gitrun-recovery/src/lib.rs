use gitrun_core::StateStore;

pub fn startup_recovery(store: &StateStore) -> Result<(), Box<dyn std::error::Error>> {
    store.write_health(true, "recovery subsystem initialized")?;
    Ok(())
}

pub fn record_failure(store: &StateStore, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    store.write_health(false, message)?;
    store.record_crash(message)?;
    Ok(())
}
