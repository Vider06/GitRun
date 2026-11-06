# Release metadata and reference artifacts

## Source versioning

The root version.txt records the current product version, <!-- gitrun-version:start -->1.9.1<!-- gitrun-version:end -->, and is the product-version source of truth. The release workflow synchronizes its marked references in Markdown, the Tauri bundle configuration, frontend package.json and package-lock.json, then checks the synchronized release workspace.

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

The 1.9.1 release workflow builds independent Server and Desktop components. Server binaries cover `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. Canonical outputs are packaged into the native formats declared by the workflow:

- DEB and RPM for x86_64/aarch64 GNU.
- Arch packages for x86_64/aarch64 GNU.
- APK for x86_64/aarch64 musl, with OpenRC service definitions.
- XBPS for x86_64/aarch64 GNU and musl, with runit service trees and lifecycle hooks.
- TAR.GZ updater archives and TAR.ZST portable archives for all four targets.

Desktop is a separate four-target GNU/musl build. GNU targets produce AppImage, DEB, RPM and Arch packages. Musl targets produce standalone binaries and XBPS packages for both architectures; APK is published for x86_64 only. Desktop packages do not bundle the Server executable.

For a specific published version, use [GitHub Releases](https://github.com/Vider06/GitRun/releases) and install only an asset actually present in that release's signed manifest. Choose the native package matching the host architecture and libc, verify its SHA-256 value against the signed manifest, install it with the host package manager, then run `gitrun setup` to configure the detected service manager. The built-in `gitrun update` command uses the signed TAR.GZ update path and does not replace the distro package; use the native package manager to upgrade a native package.

The 1.9.1 release has a one-time legacy Debian migration package for existing 1.8.23 x86_64 GNU/APT installations. It retained the old `gitrun` package identity so the 1.8.23 updater could install the new Server binary. It was not for fresh installations and is not repeated in later releases. Automatic `gitrun reinstall` package replacement currently supports x86_64 GNU/Linux with APT.

The native service lifecycle is service-manager-specific: DEB/RPM/Arch Server packages carry systemd units; Alpine APK carries OpenRC definitions; Void XBPS carries runit service trees and `INSTALL`/`REMOVE` hooks. Package hooks restart only previously active services on upgrade and preserve configuration/state on removal. The explicit `gitrun uninstall` command is the destructive cleanup path.

The separately packaged Desktop application can be installed independently of the Server package. The CLI and Dashboard use the same `gitrun-management` backend; terminal-first users can run `gitrun setup --terminal` and `gitrun manage` without installing the Tauri runtime.
