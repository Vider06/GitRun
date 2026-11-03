//! Thin Tauri desktop adapter for the shared GitRun management backend.
pub use gitrun_management::*;

/// Start the native operator dashboard. All command handlers are implemented
/// and authorized by gitrun-management; this crate owns only the Tauri window.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            is_first_run,
            secure_private_key,
            run_first_setup,
            uninstall_gitrun,
            reinstall_gitrun,
            get_overview,
            get_runner_snapshot,
            fast_stop_workflow_run,
            get_repository_activity,
            get_host_resource_snapshot,
            get_repo_detail,
            get_repo_dashboard_bootstrap,
            list_vault_secrets,
            set_vault_secret,
            delete_vault_secret,
            get_gsr_status,
            list_gsr_events,
            list_service_logs,
            get_zizmor_info,
            accept_zizmor_license_and_install,
            disable_zizmor,
            list_logic_rules,
            save_logic_rules,
            list_vm_configs,
            save_vm_config,
            delete_vm_config,
            list_pending_hypervisor_decisions,
            respond_hypervisor_decision,
            get_config,
            get_security_runtime_summary,
            get_dashboard_health,
            get_kitty_dashboard_data,
            save_kitty_settings,
            control_gitrun_service,
            check_gitrun_updates,
            update_permanent_runners,
            save_dashboard_settings,
            save_config,
            get_gitrun_settings,
            save_gitrun_settings,
            get_repo_api_policies,
            get_dock_requirements,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the GitRun dashboard");
}
