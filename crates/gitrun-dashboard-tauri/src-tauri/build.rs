fn main() {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let version_file = workspace_root.join("version.txt");
    println!("cargo:rerun-if-changed={}", version_file.display());

    let version = std::fs::read_to_string(&version_file).unwrap_or_else(|error| {
        panic!(
            "unable to read canonical GitRun version file {}: {error}",
            version_file.display()
        )
    });
    let version = version.trim();
    if version.is_empty() {
        panic!(
            "canonical GitRun version file {} is empty",
            version_file.display()
        );
    }
    println!("cargo:rustc-env=GITRUN_BUILD_VERSION={version}");

    tauri_build::build()
}
