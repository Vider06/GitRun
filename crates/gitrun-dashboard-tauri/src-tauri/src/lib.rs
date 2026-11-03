//! Thin Tauri desktop adapter for the shared GitRun management backend.
pub use gitrun_management::*;

/// Start the native operator dashboard. All command handlers are implemented
/// and authorized by gitrun-management; this crate owns only the Tauri window.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            gitrun_management::backend::is_first_run,
            gitrun_management::backend::secure_private_key,
            gitrun_management::backend::run_first_setup,
            gitrun_management::backend::uninstall_gitrun,
            gitrun_management::backend::reinstall_gitrun,
            gitrun_management::backend::get_overview,
            gitrun_management::backend::get_runner_snapshot,
            gitrun_management::backend::fast_stop_workflow_run,
            gitrun_management::backend::get_repository_activity,
            gitrun_management::backend::get_host_resource_snapshot,
            gitrun_management::backend::get_repo_detail,
            gitrun_management::backend::get_repo_dashboard_bootstrap,
            gitrun_management::backend::list_vault_secrets,
            gitrun_management::backend::set_vault_secret,
            gitrun_management::backend::delete_vault_secret,
            gitrun_management::backend::get_gsr_status,
            gitrun_management::backend::list_gsr_events,
            gitrun_management::backend::list_service_logs,
            gitrun_management::backend::get_zizmor_info,
            gitrun_management::backend::accept_zizmor_license_and_install,
            gitrun_management::backend::disable_zizmor,
            gitrun_management::backend::list_logic_rules,
            gitrun_management::backend::save_logic_rules,
            gitrun_management::backend::list_vm_configs,
            gitrun_management::backend::save_vm_config,
            gitrun_management::backend::delete_vm_config,
            gitrun_management::backend::list_pending_hypervisor_decisions,
            gitrun_management::backend::respond_hypervisor_decision,
            gitrun_management::backend::get_config,
            gitrun_management::backend::get_security_runtime_summary,
            gitrun_management::backend::get_dashboard_health,
            gitrun_management::backend::get_kitty_dashboard_data,
            gitrun_management::backend::save_kitty_settings,
            gitrun_management::backend::control_gitrun_service,
            gitrun_management::backend::check_gitrun_updates,
            gitrun_management::backend::update_permanent_runners,
            gitrun_management::backend::save_dashboard_settings,
            gitrun_management::backend::save_config,
            gitrun_management::backend::get_gitrun_settings,
            gitrun_management::backend::save_gitrun_settings,
            gitrun_management::backend::get_repo_api_policies,
            gitrun_management::backend::get_dock_requirements,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the GitRun dashboard");
}
