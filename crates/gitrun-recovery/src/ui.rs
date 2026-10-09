#[tauri::command]
fn get_report() -> Result<super::RecoveryReport, String> {
    Ok(super::inspect())
}

#[tauri::command]
fn run_gtuu() -> Result<super::RecoveryReport, String> {
    let mut structured: Option<gitrun_scheduler::GtuuStartupReport> = None;
    let mut cli_summary: Option<String> = None;
    if super::is_root_for_ui() {
        structured = Some(super::run_gtuu()?);
    } else {
        let gitrun = super::find_gitrun_binary()
            .ok_or("GitRun executable was not found for privileged GTUU")?;
        let output = std::process::Command::new("pkexec")
            .arg(gitrun)
            .arg("recovery-gtuu")
            .output()
            .map_err(|error| format!("unable to request privileged GTUU: {error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if detail.is_empty() {
                format!("privileged GTUU exited with {}", output.status)
            } else {
                format!("privileged GTUU exited with {}: {}", output.status, detail)
            });
        }
        cli_summary = Some(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }

    let mut report = super::inspect();
    if let Some(gtuu) = structured {
        let target_changed = gtuu.target_version.as_deref()
            .map(|version| version != report.version)
            .unwrap_or(false);
        let applied = gtuu.gitrun_updated;
        report.update = super::UpdateStatus {
            checked: true,
            available: target_changed && !applied,
            current_version: if applied {
                gtuu.target_version.clone().unwrap_or(gtuu.current_version.clone())
            } else {
                gtuu.current_version.clone()
            },
            target_version: gtuu.target_version.clone(),
            applied,
            error: gtuu.gitrun_update_error.clone()
                .or(gtuu.runner_image_error.clone())
                .or(gtuu.containers_error.clone()),
        };
        for (code, title, error) in [
            ("gtuu-gitrun", "GitRun update failed", gtuu.gitrun_update_error),
            ("gtuu-runner-image", "Runner image update failed", gtuu.runner_image_error),
            ("gtuu-containers", "Runner container reconciliation failed", gtuu.containers_error),
        ] {
            if let Some(detail) = error {
                report.issues.push(super::RecoveryIssue {
                    code: code.into(),
                    severity: super::Severity::Warning,
                    title: title.into(),
                    detail,
                    repairable: true,
                });
            }
        }
    } else {
        let summary = cli_summary.unwrap_or_default();
        report.update = super::UpdateStatus {
            checked: true,
            available: false,
            current_version: report.version.clone(),
            target_version: None,
            applied: false,
            error: if summary.is_empty() {
                None
            } else {
                Some(format!("GTUU completed: {}", summary))
            },
        };
    }
    report.healthy = !report.has_critical();
    Ok(report)
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
