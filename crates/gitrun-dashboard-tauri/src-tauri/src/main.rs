// Desktop entry point. GitRun intentionally does not target Android builds.
fn main() {
    if let Err(reason) = gitrun_platform::ensure_supported_runtime() {
        eprintln!("GitRun Dashboard: {reason}");
        std::process::exit(2);
    }
    gitrun_dashboard_tauri_lib::run()
}
