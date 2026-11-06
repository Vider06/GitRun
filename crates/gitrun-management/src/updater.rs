//! GUI/TUI adapter for updater-owned policy persistence and diagnostics.
//! The updater crate owns policy, host discovery, storage and operation history.

pub use gitrun_updater::state::{
    UpdateCheckInfo, UpdateLockStatus, UpdateOperation, UpdatePolicyChoice,
    UpdateServiceStatus, UpdaterInfo, UpdaterSettings,
};

pub fn current_settings_local() -> Result<UpdaterSettings, String> {
    gitrun_updater::state::current_settings_local()
}

pub fn local_settings_save(policy: &str) -> Result<UpdaterSettings, String> {
    gitrun_updater::state::local_settings_save(policy)
}

pub fn info_local(check: bool) -> Result<UpdaterInfo, String> {
    gitrun_updater::state::info_local(check)
}

pub fn record_cli_operation(
    from_version: &str,
    to_version: Option<&str>,
    release_type: Option<&str>,
    exit_code: i32,
) {
    gitrun_updater::state::record_cli_operation(from_version, to_version, release_type, exit_code);
}
