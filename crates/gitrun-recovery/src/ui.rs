#[tauri::command]
fn get_report() -> Result<super::RecoveryReport, String> {
    Ok(super::inspect())
}

#[tauri::command]
fn run_gtuu() -> Result<super::RecoveryReport, String> {
    if super::is_root_for_ui() {
        super::run_gtuu()?;
    } else {
        let gitrun = super::find_gitrun_binary()
            .ok_or("GitRun executable was not found for privileged GTUU")?;
        let status = std::process::Command::new("pkexec")
            .arg(gitrun)
            .arg("recovery-gtuu")
            .status()
            .map_err(|error| format!("unable to request privileged GTUU: {error}"))?;
        if !status.success() {
            return Err(format!("privileged GTUU exited with {status}"));
        }
    }
    Ok(super::inspect())
}

#[tauri::command]
fn repair_service() -> Result<(), String> {
    super::restart_service()
}

#[tauri::command]
fn open_dashboard() -> Result<(), String> {
    super::launch_dashboard().map(|_| ())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_report,
            run_gtuu,
            repair_service,
            open_dashboard
        ])
        .run(tauri::generate_context!())
        .expect("error while running GitRun Recovery UI");
}
