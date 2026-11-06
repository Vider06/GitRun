# `scripts`

Installation, release-build and verification scripts for Linux, macOS and Windows. Scripts are operational tooling rather than part of the Rust runtime. Check installer and release behavior against the current source/product version in `version.txt` (currently <!-- gitrun-version:start -->1.9.1<!-- gitrun-version:end --> on this source branch). The 1.8.16 source line adds GTUU read-only checks, forced validated local runner-image rebuilding, safe permanent-runner replacement and automatic sudo authorization for system installations. The 1.8.15 source line fingerprints local runner-image build inputs, validates a temporary image against the runtime GSR policy bootstrap and triggers GTUU reconciliation. The 1.8.14 source line added acknowledged Windows/Linux Logs clipboard helpers with a Save As fallback and repaired the GSR bootstrap entrypoint across runner profiles. The 1.8.12 source line improved native clipboard delivery and tmux OSC 52 passthrough. Use `bash scripts/gitrun-manage.sh --target user@host` on Linux or `powershell.exe -ExecutionPolicy Bypass -File .\\scripts\\gitrun-manage.ps1 -Target user@host` on Windows when you want local clipboard acknowledgement and interactive file export. The 1.8.11 source line corrected the transient restore timeout and hardened recovery of the workflow API socket path. The 1.8.10 source line fixed Docker's built-in seccomp default handling and improved diagnostics for failed isolated Main Library restoration. It builds on the 1.8.9 signed Main Library restore path and protected JSON reporting, the 1.8.8 CI cache-fingerprint helper, and earlier guarded JSON recovery, host CPU history, runner-network bootstrap fix and TUI Logs clipboard shortcut. Their behavior and limits are recorded in [the consolidated release history](../docs/RELEASE_HISTORY.md); Kitty queue-evaluation remains documented in [the Smart Scheduler guide](../docs/SMART_SCHEDULER.md), while protected dashboard settings and earlier runtime migration are historical records. Historical verification limits are summarized in [release history](../docs/RELEASE_HISTORY.md); they do not validate a new installer or release commit.

## Installing a published Server package

Choose the **GitRun-Server** asset matching the host distribution, architecture and libc from the signed [GitHub Release](https://github.com/Vider06/GitRun/releases) manifest. The current release workflow defines these Server package outputs:

| Package | Targets | Service manager |
| --- | --- | --- |
| DEB / RPM | x86_64 and aarch64 GNU | systemd |
| Arch | x86_64 and aarch64 GNU | systemd |
| APK | x86_64 and aarch64 musl | OpenRC |
| XBPS | x86_64 and aarch64 GNU/musl | runit |

The workflow also publishes TAR.GZ updater archives and TAR.ZST portable archives for all four GNU/musl architecture targets. These are separate artifacts from native packages. The built-in `gitrun update` path installs the signed archive; it does not replace the distro package. Use the host package manager when updating a native package.

For example, on Debian/Ubuntu:

```bash
VERSION="vX.Y.Z" # choose a published stable tag from GitHub Releases
ASSET="GitRun-Server-${VERSION}-linux-x86_64-deb.deb"
curl -fL --proto '=https' --tlsv1.2 \
  "https://github.com/Vider06/GitRun/releases/download/${VERSION}/${ASSET}" \
  -o "${ASSET}"
sha256sum "${ASSET}" # compare with the signed release manifest
sudo apt install "./${ASSET}"
sudo gitrun setup --terminal
gitrun doctor
```

The matching package manager resolves declared dependencies. Docker and GitHub credentials remain operator-managed prerequisites. After first installation, `gitrun setup` configures and enables the detected service manager. Package installation alone does not start GitRun.

Alpine APK packages carry OpenRC service definitions. APK removal stops and unregisters GitRun services; APK upgrades restart only services that were already active. Void XBPS packages carry runit service trees and `INSTALL`/`REMOVE` hooks; setup creates the `/var/service` links, upgrades restart only previously running services, and removal stops GitRun and removes only its setup-created links. Package hooks preserve configuration and state; use the explicit `gitrun uninstall` command for destructive cleanup.

The automatic `gitrun reinstall` package-replacement path is currently limited to x86_64 GNU/Linux with APT. The one-time 1.9.0 migration DEB is only for existing 1.8.23 x86_64 Debian/Ubuntu installations and must never be used for a fresh install. The separate Desktop packages do not include the Server executable.

## Main Library release archive

The release workflow creates `library.zip` from the explicit JSON reference-source list in `.github/workflows/release.yml`, storing each source as a sibling-style `.json.Back` reference while preserving its relative path in the ZIP. The archive is expected to contain 11 unique, safe members. The workflow validates the count, path shape and JSON type before publication; the signed release manifest records its SHA-256 checksum. Installation and restoration also verify the manifest/checksum and bound archive extraction. Keep this reference archive separate from active configuration: normal setup/update fills missing Main Library files and creates missing local backups without replacing existing copies, while explicit recovery is the operation that can replace the reference-library set.

## Current Markdown version markers

`check-version-consistency.py` validates that every local Cargo package version is represented in `Cargo.lock`; the release workflow additionally checks that Tauri, `package.json`, and `package-lock.json` agree with `version.txt`. CI runs the Cargo-only check so crate versions remain independently versioned rather than being forced to equal the product version. The release workflow can also synchronize version markers in the separate GitHub Wiki repository when a repository secret named `WIKI_SYNC_TOKEN` has write access to that Wiki; it changes only existing paired markers and skips the Wiki when the secret is not configured.\n\n`sync-markdown-version.py` keeps the visible current-source version labels synchronized with `version.txt`. GitHub renders Markdown but does not expand arbitrary environment variables, so the script updates values stored inside invisible HTML comments rather than leaving placeholders visible to readers.

- Run `python3 scripts/sync-markdown-version.py` to update the marked values from `version.txt`.
- Run `python3 scripts/sync-markdown-version.py --check` to detect markers whose displayed values differ from the requested/current product version.
- Run `python3 scripts/sync-markdown-version.py --check-structure` in general PR CI to validate paired markers and required coverage while allowing version synchronization to remain deferred.
- The release workflow updates markers to the selected release version and runs strict `--check` before packaging. If `main/version.txt` still matches that release version, it creates or refreshes a dedicated automation branch from current `main` and opens (or updates) a docs-only pull request. It never pushes the documentation commit directly to `main`. If `main` targets a different product version, the workflow synchronizes only its release workspace and logs a warning. Historical release numbers outside the marked current-source references are intentionally preserved.
