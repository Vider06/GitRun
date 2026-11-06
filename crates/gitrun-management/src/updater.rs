//! GUI/TUI adapter for updater-owned policy persistence and diagnostics.
//! The updater crate owns policy, host discovery, storage and operation history.

pub use gitrun_updater::state::{
    UpdateCheckInfo, UpdateLockStatus, UpdateOperation, UpdatePolicyChoice, UpdateServiceStatus,
    UpdaterInfo, UpdaterSettings,
};

pub fn get_updater_settings() -> Result<UpdaterSettings, String> {
    gitrun_updater::state::read_settings()
}

pub fn save_updater_settings(policy: &str) -> Result<UpdaterSettings, String> {
    gitrun_updater::state::save_settings(policy)
}

pub fn get_updater_info(check: bool) -> Result<UpdaterInfo, String> {
    gitrun_updater::state::info(check)
}

pub fn record_cli_operation(
    from_version: &str,
    to_version: Option<&str>,
    release_type: Option<&str>,
    exit_code: i32,
) {
    gitrun_updater::state::record_cli_operation(
        from_version,
        to_version,
        release_type,
        exit_code,
    );
}
