use gitrun_updater::load_manifest;
use std::env;

#[test]
fn verifies_release_manifest_signature_when_fixture_is_provided() {
    let Ok(path) = env::var("GITRUN_RELEASE_MANIFEST") else {
        return;
    };
    let expected_key_id =
        env::var("GITRUN_UPDATE_PUBLIC_KEY_ID").expect("public key id must be configured");

    let manifest = load_manifest(path).expect("GitRun updater must accept the release signature");
    assert_eq!(manifest.name, "GitRun");
    assert!(manifest.signature.is_some());
    assert!(
        manifest.signature_v2.is_some(),
        "new release manifests must carry the full-payload v2 signature"
    );
    assert_eq!(
        manifest.signature_key_id.as_deref(),
        Some(expected_key_id.as_str())
    );

    let mut tampered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read signed manifest"))
            .expect("parse signed manifest");
    let original_type = tampered["release_type"].as_str().unwrap_or("pre").to_owned();
    tampered["release_type"] = serde_json::Value::String(
        if original_type == "pre" { "full" } else { "pre" }.to_owned(),
    );
    let tampered_path = std::env::temp_dir().join(format!(
        "gitrun-tampered-release-type-{}.json",
        std::process::id()
    ));
    std::fs::write(
        &tampered_path,
        serde_json::to_vec(&tampered).expect("serialize tampered manifest"),
    )
    .expect("write tampered manifest");
    assert!(
        load_manifest(&tampered_path).is_err(),
        "updater must reject release_type changes even when the legacy signature remains valid"
    );
    let _ = std::fs::remove_file(tampered_path);
}
