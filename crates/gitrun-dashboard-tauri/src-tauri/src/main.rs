// Desktop entry point. Per Tauri 2 convention, actual app logic lives in
// lib.rs (shared with mobile builds, which GitRun doesn't target today but
// keeping the split costs nothing and matches the ecosystem's expectations).
fn main() {
    gitrun_dashboard_tauri_lib::run()
}
