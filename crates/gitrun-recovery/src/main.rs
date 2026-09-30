use std::process::Command;

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "start".into());
    let target = args.next().unwrap_or_else(|| "dashboard".into());

    let code = match command.as_str() {
        "start" => start(&target),
        "check" => check(),
        "gtuu" => gtuu(),
        "repair-service" => repair_service(),
        "ui" => {
            gitrun_recovery::ui::run();
            0
        }
        _ => {
            eprintln!("GitRun Recovery");
            eprintln!("usage: gitrun-recovery start <scheduler|dashboard>");
            eprintln!("       gitrun-recovery check");
            eprintln!("       gitrun-recovery gtuu");
            eprintln!("       gitrun-recovery repair-service");
            eprintln!("       gitrun-recovery ui");
            2
        }
    };
    std::process::exit(code);
}

fn start(target: &str) -> i32 {
    let target = match target {
        "scheduler" => gitrun_recovery::StartupTarget::Scheduler,
        "dashboard" => gitrun_recovery::StartupTarget::Dashboard,
        _ => {
            eprintln!("unknown startup target: {target}");
            return 2;
        }
    };

    match gitrun_recovery::startup(target) {
        Ok(report) if report.has_critical() => {
            eprintln!("GitRun Recovery: startup blocked by a critical issue.");
            for issue in &report.issues {
                if issue.severity == gitrun_recovery::Severity::Critical {
                    eprintln!("  {}: {}", issue.title, issue.detail);
                }
            }
            if target == gitrun_recovery::StartupTarget::Dashboard && gui_available() {
                let _ = launch_ui();
            }
            1
        }
        Ok(_) => {
            if let Err(error) = gitrun_recovery::mark_startup_healthy() {
                eprintln!("GitRun Recovery: unable to persist startup health: {error}");
            }

            let status = match target {
                gitrun_recovery::StartupTarget::Scheduler => gitrun_recovery::launch_scheduler(),
                gitrun_recovery::StartupTarget::Dashboard => gitrun_recovery::launch_dashboard(),
            };

            match status {
                Ok(status) if status.success() => 0,
                Ok(status) => {
                    let message = format!("GitRun child process exited with status {status}");
                    let _ = gitrun_recovery::record_failure(&message);
                    if target == gitrun_recovery::StartupTarget::Dashboard && gui_available() {
                        let _ = launch_ui();
                    }
                    status.code().unwrap_or(1)
                }
                Err(error) => {
                    let _ = gitrun_recovery::record_failure(&error);
                    if gui_available() {
                        let _ = launch_ui();
                    }
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("GitRun Recovery: {error}");
            if gui_available() {
                let _ = launch_ui();
            }
            1
        }
    }
}

fn check() -> i32 {
    let report = gitrun_recovery::inspect();
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
    );
    if report.has_critical() {
        1
    } else {
        0
    }
}

fn gtuu() -> i32 {
    match gitrun_recovery::run_gtuu() {
        Ok(report) => {
            println!(
                "GTUU: GitRun {}{}; {} permanent runner(s) updated",
                report.current_version,
                report
                    .target_version
                    .as_deref()
                    .map(|version| format!(" -> {version}"))
                    .unwrap_or_default(),
                report.containers_updated
            );
            if let Some(error) = report.gitrun_update_error {
                eprintln!("GitRun update: {error}");
                return 1;
            }
            if let Some(error) = report.runner_image_error {
                eprintln!("Runner image: {error}");
                return 1;
            }
            if let Some(error) = report.containers_error {
                eprintln!("Containers: {error}");
                return 1;
            }
            0
        }
        Err(error) => {
            eprintln!("GTUU: FAIL — {error}");
            1
        }
    }
}

fn repair_service() -> i32 {
    match gitrun_recovery::repair_service_unit() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GitRun Recovery service repair: FAIL — {error}");
            1
        }
    }
}

fn gui_available() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn launch_ui() -> Result<std::process::ExitStatus, String> {
    let recovery = std::env::var("GITRUN_RECOVERY_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/gitrun-recovery"));
    Command::new(recovery)
        .arg("ui")
        .status()
        .map_err(|error| format!("unable to launch recovery UI: {error}"))
}
