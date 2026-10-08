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
    assert_eq!(manifest.signature_key_id.as_deref(), Some(expected_key_id.as_str()));
}
