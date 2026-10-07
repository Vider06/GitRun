use std::process::Command;

#[test]
fn piped_gitrun_output_contains_no_cat_or_ansi_sequences() {
    let output = Command::new(env!("CARGO_BIN_EXE_gitrun"))
        .arg("api-list")
        .env_remove("GITRUN_NO_CAT")
        .env_remove("CI")
        .output()
        .expect("failed to run gitrun api-list");

    assert!(output.status.success(), "gitrun api-list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stdout.contains("\x1b["), "stdout unexpectedly contains ANSI escapes: {stdout:?}");
    assert!(!stderr.contains("\x1b["), "stderr unexpectedly contains ANSI escapes: {stderr:?}");
    assert!(!stdout.contains("/\\_/\\"), "stdout unexpectedly contains the cat sprite: {stdout:?}");
    assert!(!stderr.contains("/\\_/\\"), "stderr unexpectedly contains the cat sprite: {stderr:?}");
}