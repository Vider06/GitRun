#!/usr/bin/env python3
"""Static contracts for the classified GitRun updater release flow."""

from pathlib import Path
import json
import re

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


schema = json.loads(read("release/release-manifest.schema.json"))
example = json.loads(read("release/release-manifest.example.json"))
release = read(".github/workflows/release.yml")
promotion = read(".github/workflows/release-type-change.yml")
engine = read("crates/gitrun-updater/src/release_policy.rs")
manifest_engine = read("crates/gitrun-updater/src/lib.rs")
manifest_engine_normalized = " ".join(manifest_engine.split())
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
    "promotion workflow checks the compiled-in public trust anchor": 'expected_public_bytes="17 204 158 175 1 71 218 218 64 127 253 52 151 194 206 188 225 125 25 94 33 192 96 48 193 226 225 220 104 143 65 241"' in promotion
    and 'tail -c 32 "$public" | od -An -v -tu1' in promotion
    and "gitrun-release-ed25519-v1" in promotion
    and "DEFAULT_PUBLIC_KEY_BYTES: [u8; 32]" in read("crates/gitrun-updater/src/state.rs"),
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
    "normal release selection rejects downgrades": "pub fn select_releases" in engine
    and "== Ordering::Greater" in engine,
    "line transition uses only typed release classifications": "MIN_CLASSIFIED_RELEASE_VERSION" in engine
    and "pub fn select_line_transition_releases" in engine,
    "line transition never crosses the latest release direction or downgrades from an equal latest version": "direction == Ordering::Equal" in engine
    and "!= direction" in engine
    and "line_transition_does_not_downgrade_when_latest_line_version_is_already_installed" in engine,
    "line transition has a separate plan that preserves ordinary monotonic updates": "pub fn build_line_transition_plan" in manifest_engine
    and re.search(
        r"pub fn build_plan\(.*?build_plan_with_version_policy\(\s*manifest,\s*current_version,\s*target,\s*installed_dependencies,\s*false,\s*\)",
        manifest_engine_normalized,
    )
    is not None
    and re.search(
        r"pub fn build_line_transition_plan\(.*?build_plan_with_version_policy\(\s*manifest,\s*current_version,\s*target,\s*installed_dependencies,\s*true,\s*\)",
        manifest_engine_normalized,
    )
    is not None
    and "allow_older_version: bool" in manifest_engine
    and "ordering == std::cmp::Ordering::Equal || (!allow_older_version && ordering != std::cmp::Ordering::Greater)"
    in manifest_engine_normalized
    and "fn explicit_line_transition_allows_older_version_but_not_equal_version" in manifest_engine,
    "line transition resolves a latest signed release and checks version/type metadata": "resolve_policy_update_for_line_transition_target" in engine
    and "super::build_line_transition_plan" in engine
    and "manifest.release_type != release.release_type" in engine,
    "release selection verifies manifest classification": "manifest.release_type != release.release_type" in engine,
    "host compatibility is checked before selecting a release": "resolve_policy_update_for_target" in engine
    and "UpdateError::UnsupportedTarget" in engine,
    "release classification participates in signed manifest data": "pub release_type: ReleaseType" in manifest_engine
    and "unsigned.signature = None" in manifest_engine,
    "updater crate owns persistent policy and diagnostics": "pub mod state;" in manifest_engine
    and "pub fn info_local" in read("crates/gitrun-updater/src/state.rs")
    and "pub fn read_settings()" in read("crates/gitrun-updater/src/state.rs")
    and "pub fn save_settings(" in read("crates/gitrun-updater/src/state.rs"),
    "management delegates state to updater": "gitrun_updater::state::info(" in management_updater
    and "gitrun_updater::state::read_settings()" in management_updater
    and "gitrun_updater::state::save_settings(" in management_updater
    and "gitrun_updater::state::record_cli_operation" in management_updater
    and "updater::get_updater_info(check)" in management
    and "updater::get_updater_settings()" in management,
    "updater state writes serialize across processes": "libc::flock" in read("crates/gitrun-updater/src/state.rs")
    and '".updater-state.lock"' in read("crates/gitrun-updater/src/state.rs")
    and "libc::LOCK_EX" in read("crates/gitrun-updater/src/state.rs")
    and "UPDATER_STATE_WRITE_LOCK" in read("crates/gitrun-updater/src/state.rs"),
    "Control Manager exposes only fixed updater IPC actions": '"updater_settings"' in control_manager
    and '"updater_info"' in control_manager
    and '"updater_settings_save"' in control_manager
    and '"updater_operation_record"' in control_manager
    and "CONTROL_MANAGER_MAX_MESSAGE" in read("crates/gitrun-updater/src/state.rs"),
    "CLI exposes info and combined info check": "info: bool" in cli
    and "fn run_update_info" in cli
    and "read_updater_info(perform_check)" in cli,
    "CLI queries updater crate directly": "gitrun_updater::state::read_settings()" in cli
    and "gitrun_updater::state::info(check)" in cli
    and "gitrun_management::get_updater_info" not in cli
    and "gitrun_management::get_updater_settings" not in cli,
    "TUI uses shared updater settings actions": "updater_settings" in tui
    and "updater_settings_save" in tui
    and "choose_update_policy" in tui,
    "GUI calls management commands instead of owning updater logic": 'invoke("get_updater_settings")' in gui
    and 'invoke("save_updater_settings"' in gui
    and 'invoke("get_updater_info"' in gui,
        "line transition is available only through a hidden explicit CLI mode": "hide = true" in cli
    and "line_transition: bool" in cli
    and '"--line-transition"' in cli
    and "an explicit manifest URL cannot be used for a release-line transition" in cli,
    "CLI transition mode has parser tests and rejects conflicting modes": "fn line_transition_flag_is_explicit_and_cannot_be_combined" in cli
    and "build_line_transition_plan(&manifest" in cli,
    "Management TUI executes user-confirmed operations in the foreground": "fn run_confirmed_command" in tui
    and "run_external(terminal, command_args)" in tui
    and "maintenance_task_with_confirmation" not in tui
    and "Would you like to instantly roll to the latest line version?" in tui,
    "Dashboard runs updater transitions as a streamed foreground command": "async fn update_gitrun" in management
    and "        update_gitrun," in management
    and 'invoke("update_gitrun", {lineTransition})' in gui
    and 'eventApi.listen("gitrun-setup-progress"' in gui,
    "direct GUI maintenance actions no longer enqueue Control Manager maintenance jobs": '"maintenance_task"' not in management,
    "Control Manager retains its separate scheduled maintenance mechanism": "schedule_maintenance_task" in control_manager
    and '"maintenance_task"' in control_manager,
}

failed = []
for label, passed in checks.items():
    print(("PASS" if passed else "FAIL") + ": " + label)
    if not passed:
        failed.append(label)

if failed:
    raise SystemExit("Updater release-policy contracts failed: " + "; ".join(failed))

print("Updater release-policy contracts passed.")
