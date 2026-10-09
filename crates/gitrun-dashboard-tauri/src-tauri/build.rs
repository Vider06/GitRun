fn main() {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let version_file = workspace_root.join("version.txt");
    println!("cargo:rerun-if-changed={}", version_file.display());

    if let Ok(version) = std::fs::read_to_string(&version_file) {
        let version = version.trim();
        if !version.is_empty() {
            println!("cargo:rustc-env=GITRUN_BUILD_VERSION={version}");
        }
    }

    tauri_build::build()
}
