# Release metadata and reference artifacts

## Source versioning

The root version.txt records the current product version, <!-- gitrun-version:start -->1.8.21<!-- gitrun-version:end -->, and is the product-version source of truth. The release workflow synchronizes its marked references in Markdown, the Tauri bundle configuration, frontend package.json and package-lock.json, then checks the synchronized release workspace.

Cargo crates use independent SemVer versions. Bump only crates affected by a change and keep Cargo.lock aligned with their manifests. The frontend package lock must likewise match its package manifest.

A source-version update, merged pull request or successful build does not prove that a signed release artifact has been published. Check [GitHub Releases](https://github.com/Vider06/GitRun/releases) before installing or deploying a release.

For source-line scope and historical verification boundaries, use the [consolidated release history](../docs/RELEASE_HISTORY.md). Kitty behavior and its operational limits are documented in the [Kitty Smart Scheduler guide](../docs/SMART_SCHEDULER.md).

## Main Library reference artifact

The release workflow publishes library.zip as the library-json-backup target in the signed release-manifest.json. The archive contains 11 selected JSON reference inputs, each stored with a .json.Back suffix:

- Core/Dockers/runners/linux-x86_64/profiles.json.Back
- crates/gitrun-cli/src/cat/cat_states.json.Back
- crates/gitrun-cli/src/cat/gitrun.json.Back
- crates/gitrun-dashboard-tauri/package.json.Back
- crates/gitrun-dashboard-tauri/package-lock.json.Back
- crates/gitrun-dashboard-tauri/src-tauri/capabilities/default.json.Back
- crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json.Back
- crates/gitrun-recovery/capabilities/default.json.Back
- crates/gitrun-recovery/tauri.conf.json.Back
- release/release-manifest.example.json.Back
- release/release-manifest.schema.json.Back

The release check requires 11 distinct, safe member paths and valid JSON object/array content. The updater and recovery path verify the signed manifest and archive checksum, then enforce archive path and expansion limits before restoration.

Routine setup and update operations fill missing reference-library entries and create only missing local backups; they do not overwrite existing entries. The explicit Restore Main Library operation restores the dedicated reference library and does not replace active operator configuration.

## Deployment and Debian package installation

Each stable release publishes a prebuilt Debian package for amd64/x86-64 named `GitRun-vVERSION-linux-x86_64-deb.deb`, alongside the signed release manifest and its checksum metadata. For standard Debian/Ubuntu installations, use that package rather than cloning the source tree and running the source installer.

Example for v1.8.21 (check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for the latest stable tag):

```bash
VERSION="v1.8.21"
ASSET="GitRun-${VERSION}-linux-x86_64-deb.deb"
curl -fL --proto '=https' --tlsv1.2 \
  "https://github.com/Vider06/GitRun/releases/download/${VERSION}/${ASSET}" \
  -o "${ASSET}"
sudo apt install "./${ASSET}"
gitrun setup --terminal
gitrun doctor
```

The package targets Debian/Ubuntu-compatible amd64 systems. It does not install Docker or configure GitHub credentials on the operator's behalf; those are deployment prerequisites and first-run setup tasks. APT resolves declared package dependencies when installing the local package.

Validate release-signing configuration, rollback behavior and target-host health before rollout. Keep the distinction between source metadata, signed release assets and installed runtime state explicit when diagnosing an installation.
