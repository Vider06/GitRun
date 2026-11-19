from pathlib import Path

release = Path(".github/workflows/release.yml").read_text(encoding="utf-8")
ci = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
cli = Path("crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
release_job = release.rsplit("  release:", 1)[1]
release_steps = [
    line.split("name: ", 1)[1]
    for line in release_job.splitlines()
    if line.startswith("      - name: ")
]
publish_index = release.index("      - name: Publish all release assets")
checks = {
    "legacy Debian package identity": "Package: gitrun" in release and 'artifact="GitRun-$VERSION-linux-x86_64-deb.deb"' in release,
    "legacy updater target retained": 'artifact_for("linux-x86_64-deb")' in release,
    "legacy updater source tag": "--branch v1.8.23" in release,
    "legacy trust anchor is matched to the release signing key": "test \"$GITRUN_UPDATE_PUBLIC_KEY_HEX\" = \"$legacy_key_hex\"" in release and "test \"$GITRUN_UPDATE_SIGNING_KEY_ID\" = \"$legacy_key_id\"" in release,
    "actual legacy reinstall command is executed": '--no-cat --reinstall-latest-root' in release,
    "legacy updater uses latest manifest resolution": 'latest_manifest_download_url("Vider06/GitRun")' in release,
    "migration preserves service state during legacy package replacement": "gitrun-v1.8.23-packaging/packaging/gitrun-postinst" in release and "packaging/gitrun-migration-prerm" in release and "in-favour" in release and "gitrun-server" in release and "preserves existing systemd service state" in release,
    "legacy updater SHA verification": "download_and_verify(" in release and "&artifact.sha256" in release,
    "migration has GTK and GLib build dependencies": all(package in release for package in ["libglib2.0-dev", "libgtk-3-dev", "libwebkit2gtk-4.1-dev"]),
    "migration uses a local staged-payload fixture": "GITRUN_MIGRATION_TEST_BASE_URL" in release and "python3 -m http.server" in release,
    "migration is verified before publication": release.index("Verify migration with the actual 1.8.23 updater") < publish_index,
    "release publication is the final release step": bool(release_steps) and release_steps[-1] == "Publish all release assets",
    "release manifest declares its signed classification": '"release_type":"pre"' in release and '"release_type"' in Path("release/release-manifest.schema.json").read_text(encoding="utf-8"),
    "manual release reclassification updates the signed manifest and GitHub metadata": Path(".github/workflows/release-type-change.yml").is_file() and "pkeyutl -sign" in Path(".github/workflows/release-type-change.yml").read_text(encoding="utf-8") and "GITRUN_RELEASE_TYPE:" in Path(".github/workflows/release-type-change.yml").read_text(encoding="utf-8"),
    "new updater policy is implemented in gitrun-updater": "resolve_policy_update_for_target" in Path("crates/gitrun-updater/src/release_policy.rs").read_text(encoding="utf-8"),
    "GUI and TUI use the shared management updater backend": "get_updater_settings" in Path("crates/gitrun-management/src/backend.rs").read_text(encoding="utf-8") and "save_updater_settings" in Path("crates/gitrun-dashboard-tauri/dist/js/views/settings.js").read_text(encoding="utf-8") and "updater_settings_save" in Path("crates/gitrun-cli/src/manage.rs").read_text(encoding="utf-8"),
    "CLI exposes local updater diagnostics and optional check": "info: bool" in cli and "fn run_update_info" in cli and "resolve_policy_update_for_target" in cli,
    "no post-publication migration rollback remains": "Revert failed migration verification to prerelease" not in release and "make_latest=false" not in release,
    "new releases are initially prereleases": "<!-- GITRUN_RELEASE_TYPE: pre -->" in release and 'gh release edit "$VERSION"' in release and 'gh release create "$VERSION"' in release and release.count("--prerelease") >= 2,
    "new Server package identity": "name: gitrun-server" in release and "GitRun-Server-$VERSION-" in release,
    "CI formal verification waits for successful Rust lint": "needs: [cargo-lock, rust-lint]" in ci and "needs.rust-lint.result == 'success'" in ci,
    "CI inventories every tracked Rust source file": "verify-rust-source-coverage.bash" in ci and "git ls-files -z -- '*.rs'" in Path("scripts/ci/verify-rust-source-coverage.bash").read_text(encoding="utf-8"),
    "Release repeats the Rust source inventory before signing": "verify-rust-source-coverage.bash" in release and "needs: [prepare, formal-verification]" in release,
    "CI builds and packages all four Server target variants independently of Release": "release-package-certification:" in ci and all(target in ci for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]) and "server-build-${{ matrix.target }}" in ci and "nfpm package" in ci,
    "CI packages independent GNU Desktop DEB, RPM and Arch artifacts for x86_64 and ARM64": "release-dashboard-package-certification:" in ci and all(token in ci for token in ["GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-deb.deb", "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-rpm.rpm", "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-archlinux.pkg.tar.zst"]),
    "CI builds and packages the Desktop musl matrix with Alpine APK and both-architecture XBPS": "release-dashboard-musl-build-certification:" in ci and "release-dashboard-musl-package-certification:" in ci and all(token in ci for token in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "-musl-apk.apk", "-musl-xbps.xbps"]),
    "CI conditionally builds and tests the one-time 1.9.1 legacy migration DEB": "migration-package-certification:" in ci and "if [ \"$VERSION\" = v1.9.1 ]" in ci and "gitrun-v1.8.23-packaging" in ci and "dpkg-deb --build --root-owner-group" in ci,
    "CI does not invoke the standalone release workflow": ("uses: ./.github/workflows/" + "release.yml") not in ci and ("release-" + "certification:") not in ci,
    "Release remains an independent tag/manual workflow": "workflow_call:" not in release and "server_packages_only:" not in release,
    "Release formal verification runs after every package matrix and before signing": "formal-verification:" in release and "needs: [prepare, collect, package-server, package-dashboard, migration-package]" in release and "needs: [prepare, formal-verification]" in release,
    "Release formal verification audits each produced package without rebuilding it": "Audit every release package individually" in release and "actions/download-artifact@" in release and "cargo kani setup" in release,
    "Server package replaces legacy Debian package": "conflicts: [gitrun]" in release and "replaces: [gitrun]" in release,
    "new CLI prefers the Server DEB target": 'artifact_for("linux-x86_64-server-deb")' in cli,
    "new CLI validates gitrun-server package identity": '"gitrun-server"' in cli and "trim() != expected_package" in cli,
    "new CLI detects the selected package for reinstall": ".arg(expected_package)" in cli,
    "new CLI has reinstall target unit coverage": "prefers_new_server_deb_when_both_targets_exist" in cli and "retains_legacy_target_fallback_for_old_manifests" in cli,
    "new CLI retains legacy manifest fallback": 'artifact_for("linux-x86_64-deb")' in cli and '"gitrun"' in cli,
    "Desktop package remains independent": "name: gitrun-desktop" in release,
    "Dashboard release restores Cargo and npm caches": "Restore Dashboard Cargo and npm caches" in release and ".gitrun-cargo" in release and ".gitrun-npm-cache" in release,
    "Dashboard has native x86_64 and ARM64 GNU build runners on the glibc compatibility baseline": "runner: ubuntu-22.04-arm" in release and "runner: ubuntu-22.04" in release and "target: aarch64-unknown-linux-gnu" in release and "target: x86_64-unknown-linux-gnu" in release,
    "Dashboard GNU and musl builds and packages share unified four-target matrices": all(target in release for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]) and "kind: gnu" in release and "kind: musl" in release and "build-dashboard-musl:" not in release and "package-dashboard-musl:" not in release,
    "Dashboard ARM64 musl build uses Void native WebKitGTK packages": "ghcr.io/void-linux/void-musl-full:20260801r1" in release and "libwebkit2gtk41-devel" in release and "rustup-init" in release,
    "Dashboard GNU packages include DEB, RPM and Arch": all(token in release for token in ["-deb.deb", "-rpm.rpm", "-archlinux.pkg.tar.zst"]),
    "Dashboard musl APK is limited to Alpine x86_64 while XBPS covers both architectures": all(token in release for token in ['if [ "$DESKTOP_ARCH" = "x86_64" ]', "-musl-apk.apk", "-musl-xbps.xbps", "linux-x86_64-musl-desktop-apk", "linux-x86_64-musl-desktop-xbps", "linux-aarch64-musl-desktop-xbps"]) and "linux-aarch64-musl-desktop-apk" not in release and "Alpine ARM64 Desktop APK must not be published" in release,
    "Dashboard manifest maps every Desktop package to a unique target": all(token in release for token in ["linux-x86_64-desktop-rpm", "linux-aarch64-desktop-rpm", "linux-x86_64-desktop-arch", "linux-aarch64-desktop-arch", "linux-x86_64-musl-desktop-bin", "linux-aarch64-musl-desktop-bin", "linux-x86_64-musl-desktop-apk", "linux-x86_64-musl-desktop-xbps", "linux-aarch64-musl-desktop-xbps"]),
    "Dashboard provenance declares architecture-specific package availability": "desktop_package_formats" in release and "desktop_package_arches" in release and "aarch64-unknown-linux-musl" in release,
    "Dashboard cache key is isolated from Server builds": "-desktop-" in release and "hashFiles('Cargo.lock', 'Cargo.toml', '**/Cargo.toml', 'crates/gitrun-dashboard-tauri/package-lock.json')" in release,
    "Dashboard cache is saved after its artifact build": release.index("Upload independent Dashboard artifacts") < release.index("Save Dashboard Cargo and npm caches"),
    "Dashboard uses the Node 24-compatible setup-node action": "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020" in release,
    "Desktop excludes Server executable": "grep -Eq" in release and "usr/bin/gitrun$" in release,
    "release notes include per-architecture ABI tables": '["Architecture", "ABI", "Package format", "Artifact", "SHA-256"]' in release,
    "release notes are generated from signed manifest": "manifest = json.loads(manifest_path.read_text" in release,
    "release notes verify asset checksums": 'digest != item["sha256"]' in release,
    "migration notes include fresh-install warning": "DO NOT USE FOR FRESH INSTALL!" in release,
    "migration notice is limited to the one-time migration release": 'if migration == "true":' in release and "## Migration Release" in release and "No migration needed, updater/release system has not been changed in a way that requires a migration package." not in release,
    "manifest contains separate Server package target": 'package_target[:-4] + "-server-deb"' in release,
    "release asset inventory rejects unexpected files": "diff -u <(printf" in release,
    "manifest requires the exact target set": "actual_targets != required" in release and "len(items) != len(required)" in release,
}
for label, passed in checks.items():
    print(("PASS" if passed else "FAIL") + ": " + label)
failed = [label for label, passed in checks.items() if not passed]
if failed:
    raise SystemExit("Release contract checks failed: " + "; ".join(failed))
