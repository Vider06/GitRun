# Release metadata and reference artifacts

## Source versioning

The root version.txt records the current product version, <!-- gitrun-version:start -->1.9.0<!-- gitrun-version:end -->, and is the product-version source of truth. The release workflow synchronizes its marked references in Markdown, the Tauri bundle configuration, frontend package.json and package-lock.json, then checks the synchronized release workspace.

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

## Deployment and package outputs

The 1.9.0 release workflow builds independent Server and Desktop components. Server binaries are built for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. Canonical Server outputs are then packaged into the formats declared by the workflow: DEB and RPM for GNU x86_64/aarch64, APK for musl x86_64/aarch64, Arch for GNU x86_64, XBPS for GNU/musl x86_64/aarch64, and TAR.GZ/TAR.ZST archives for all four targets. Desktop is a separate x86_64 Linux build distributed as a DEB and AppImage.

The 1.9.0 release also has a one-time legacy Debian migration package for existing 1.8.23 installations. It retains the old `gitrun` package identity to let the 1.8.23 updater install the new Server binary; it is not for fresh installations. See [Linux platform compatibility](../docs/PLATFORM_SUPPORT.md) for deployment boundaries and the [release workflow](../.github/workflows/release.yml) for the authoritative asset list.

For a specific published version, use [GitHub Releases](https://github.com/Vider06/GitRun/releases) and install only an asset actually present in that release's signed manifest. On Debian/Ubuntu, install the matching `GitRun-Server-...-deb.deb` package with APT. Docker and GitHub credentials remain operator-managed prerequisites. A package build or source version does not by itself prove that an asset has been published or that every host lifecycle is supported.

The separately packaged Desktop application can be installed independently of the Server package. The CLI and Dashboard use the same `gitrun-management` backend; terminal-first users can run `gitrun setup --terminal` and `gitrun manage` without installing the Tauri runtime.

