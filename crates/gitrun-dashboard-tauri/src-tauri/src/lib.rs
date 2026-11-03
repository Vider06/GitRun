//! Thin Tauri desktop adapter for the shared GitRun management backend.
pub use gitrun_management::*;

/// Start the native operator dashboard. The backend crate supplies the single
/// validated command registry; this package owns only the Tauri window/context.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(gitrun_management::backend::invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running the GitRun dashboard");
}

#[cfg(test)]
mod tests {
    #[test]
    fn dashboard_uses_the_shared_management_command_registry() {
        let _handler = gitrun_management::backend::invoke_handler();
        let first_run = gitrun_management::management_read("first_run", Default::default())
            .expect("shared backend first-run read must succeed");
        assert!(first_run.is_boolean());
    }
}
