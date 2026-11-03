fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("../..");
    let version_file = workspace_root.join("version.txt");
    println!("cargo:rerun-if-changed={}", version_file.display());
    let version = std::fs::read_to_string(&version_file).unwrap_or_else(|error| {
        panic!("unable to read canonical GitRun version file {}: {error}", version_file.display())
    });
    let version = version.trim();
    assert!(!version.is_empty(), "canonical GitRun version file must not be empty");
    println!("cargo:rustc-env=GITRUN_BUILD_VERSION={version}");
}
