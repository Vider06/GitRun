#!/usr/bin/env python3
"""Static contracts for the classified GitRun updater release flow."""

from pathlib import Path
import json

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


schema = json.loads(read("release/release-manifest.schema.json"))
example = json.loads(read("release/release-manifest.example.json"))
release = read(".github/workflows/release.yml")
promotion = read(".github/workflows/release-type-change.yml")
engine = read("crates/gitrun-updater/src/release_policy.rs")
manifest_engine = read("crates/gitrun-updater/src/lib.rs")
management = read("crates/gitrun-management/src/backend.rs")
management_updater = read("crates/gitrun-management/src/updater.rs")
control_manager = read("crates/gitrun-management/src/control_manager.rs")
cli = read("crates/gitrun-cli/src/main.rs")
tui = read("crates/gitrun-cli/src/manage.rs")
gui = read("crates/gitrun-dashboard-tauri/dist/js/views/settings.js")

checks = {
    "manifest requires a typed release classification": "release_type" in schema["required"]
    and schema["properties"]["release_type"]["enum"] == ["pre", "full", "security"],
    "example manifest starts as pre": example["release_type"] == "pre",
    "release workflow creates and edits prereleases": "<!-- GITRUN_RELEASE_TYPE: pre -->" in release
    and release.count("--prerelease") >= 2,
    "promotion workflow offers all classifications": all(
        f"          - {kind}" in promotion for kind in ("pre", "full", "security")
    ),
    "promotion workflow re-signs the changed manifest": "pkeyutl -sign" in promotion
    and "pkeyutl -verify" in promotion
    and "release-manifest.json" in promotion,
    "promotion workflow checks the compiled-in public trust anchor": "EXPECTED_PUBLIC_KEY_HEX" in promotion
    and "gitrun-release-ed25519-v1" in promotion,
    "promotion workflow synchronizes description and prerelease state": "GITRUN_RELEASE_TYPE:" in promotion
    and "new_prerelease=true" in promotion
    and "new_prerelease=false" in promotion,
    "promotion replaces the canonical manifest asset and can roll back metadata": 'signed="$work/updated/release-manifest.json"' in promotion
    and 'gh release upload "$TAG" "$signed"' in promotion
    and 'body="$(cat "$old_body_file")"' in promotion,
    "release selection paginates the GitHub Releases API": "per_page={RELEASES_PER_PAGE}" in engine
    and "MAX_RELEASE_PAGES" in engine,
    "release selection checks type marker and prerelease agreement": "from_release_body" in engine
    and "release.prerelease != (release_type == ReleaseType::Pre)" in engine,
    "release selection rejects downgrades": "Ordering::Greater" in engine,
    "release selection verifies manifest classification": "manifest.release_type != release.release_type" in engine,
    "host compatibility is checked before selecting a release": "resolve_policy_update_for_target" in engine
    and "UpdateError::UnsupportedTarget" in engine,
    "release classification participates in signed manifest data": "pub release_type: ReleaseType" in manifest_engine
    and "unsigned.signature = None" in manifest_engine,
    "updater crate owns persistent policy and diagnostics": "pub mod state;" in manifest_engine
    and "pub fn info_local" in read("crates/gitrun-updater/src/state.rs")
    and "pub fn local_settings_save" in read("crates/gitrun-updater/src/state.rs"),
    "management delegates state to updater": "gitrun_updater::state::info_local" in management_updater
    and "gitrun_updater::state::record_cli_operation" in management_updater,
    "Control Manager exposes only fixed updater IPC actions": '"updater_settings"' in control_manager
    and '"updater_info"' in control_manager
    and '"updater_settings_save"' in control_manager,
    "CLI exposes info and combined info check": "info: bool" in cli
    and "fn run_update_info" in cli
    and "get_updater_info(perform_check)" in cli,
    "CLI queries updater crate directly": "gitrun_updater::state::current_settings_local()" in cli
    and "gitrun_updater::state::info_local(perform_check)" in cli
    and "gitrun_management::get_updater_info" not in cli
    and "gitrun_management::get_updater_settings" not in cli,
    "TUI uses shared updater settings actions": "updater_settings" in tui
    and "updater_settings_save" in tui
    and "choose_update_policy" in tui,
    "GUI calls management commands instead of owning updater logic": 'invoke("get_updater_settings")' in gui
    and 'invoke("save_updater_settings"' in gui
    and 'invoke("get_updater_info"' in gui,
}

failed = []
for label, passed in checks.items():
    print(("PASS" if passed else "FAIL") + ": " + label)
    if not passed:
        failed.append(label)

if failed:
    raise SystemExit("Updater release-policy contracts failed: " + "; ".join(failed))

print("Updater release-policy contracts passed.")
