# `scripts`

Installation, release-build and verification scripts for Linux, macOS and Windows. Scripts are operational tooling rather than part of the Rust runtime. Check installer and release behavior against the current source/product version in `version.txt` (currently <!-- gitrun-version:start -->1.8.22<!-- gitrun-version:end --> on this source branch). The 1.8.16 source line adds GTUU read-only checks, forced validated local runner-image rebuilding, safe permanent-runner replacement and automatic sudo authorization for system installations. The 1.8.15 source line fingerprints local runner-image build inputs, validates a temporary image against the runtime GSR policy bootstrap and triggers GTUU reconciliation. The 1.8.14 source line added acknowledged Windows/Linux Logs clipboard helpers with a Save As fallback and repaired the GSR bootstrap entrypoint across runner profiles. The 1.8.12 source line improved native clipboard delivery and tmux OSC 52 passthrough. Use `bash scripts/gitrun-manage.sh --target user@host` on Linux or `powershell.exe -ExecutionPolicy Bypass -File .\\scripts\\gitrun-manage.ps1 -Target user@host` on Windows when you want local clipboard acknowledgement and interactive file export. The 1.8.11 source line corrected the transient restore timeout and hardened recovery of the workflow API socket path. The 1.8.10 source line fixed Docker's built-in seccomp default handling and improved diagnostics for failed isolated Main Library restoration. It builds on the 1.8.9 signed Main Library restore path and protected JSON reporting, the 1.8.8 CI cache-fingerprint helper, and earlier guarded JSON recovery, host CPU history, runner-network bootstrap fix and TUI Logs clipboard shortcut. Their behavior and limits are recorded in [the consolidated release history](../docs/RELEASE_HISTORY.md); Kitty queue-evaluation remains documented in [the Smart Scheduler guide](../docs/SMART_SCHEDULER.md), while protected dashboard settings and earlier runtime migration are historical records. Historical verification limits are summarized in [release history](../docs/RELEASE_HISTORY.md); they do not validate a new installer or release commit.

## Installing a published Debian package

For standard Debian/Ubuntu amd64 hosts, prefer the prebuilt package attached to the latest stable [GitHub Release](https://github.com/Vider06/GitRun/releases). The package asset is named `GitRun-vVERSION-linux-x86_64-deb.deb` (replace `vVERSION` with the release tag, such as `v1.8.21`).

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

Check GitHub Releases for the newest version before installing. This artifact targets amd64/x86-64; it is not suitable for ARM64. Docker must be installed and configured separately. A local `.deb` install through APT resolves package dependencies.

First-run setup and day-to-day configuration can be completed in the **desktop GUI**. If you prefer the terminal, use `gitrun setup --terminal` for guided setup and `gitrun manage` for the keyboard-driven TUI. The GUI and TUI use the same validated backend operations.

Use `scripts/install-server.sh` when you deliberately need a source build rather than the published package. The macOS and Windows helper scripts have separate platform requirements; see their script-specific instructions before running them.

## Main Library release archive

The release workflow creates `library.zip` from the explicit JSON reference-source list in `.github/workflows/release.yml`, storing each source as a sibling-style `.json.Back` reference while preserving its relative path in the ZIP. The archive is expected to contain 11 unique, safe members. The workflow validates the count, path shape and JSON type before publication; the signed release manifest records its SHA-256 checksum. Installation and restoration also verify the manifest/checksum and bound archive extraction. Keep this reference archive separate from active configuration: normal setup/update fills missing Main Library files and creates missing local backups without replacing existing copies, while explicit recovery is the operation that can replace the reference-library set.

## Current Markdown version markers

`check-version-consistency.py` validates that every local Cargo package version is represented in `Cargo.lock`; the release workflow additionally checks that Tauri, `package.json`, and `package-lock.json` agree with `version.txt`. CI runs the Cargo-only check so crate versions remain independently versioned rather than being forced to equal the product version. The release workflow can also synchronize version markers in the separate GitHub Wiki repository when a repository secret named `WIKI_SYNC_TOKEN` has write access to that Wiki; it changes only existing paired markers and skips the Wiki when the secret is not configured.\n\n`sync-markdown-version.py` keeps the visible current-source version labels synchronized with `version.txt`. GitHub renders Markdown but does not expand arbitrary environment variables, so the script updates values stored inside invisible HTML comments rather than leaving placeholders visible to readers.

- Run `python3 scripts/sync-markdown-version.py` to update the marked values from `version.txt`.
- Run `python3 scripts/sync-markdown-version.py --check` to detect markers whose displayed values differ from the requested/current product version.
- Run `python3 scripts/sync-markdown-version.py --check-structure` in general PR CI to validate paired markers and required coverage while allowing version synchronization to remain deferred.
- The release workflow updates markers to the selected release version and runs strict `--check` before packaging. If `main/version.txt` still matches that release version, it creates or refreshes a dedicated automation branch from current `main` and opens (or updates) a docs-only pull request. It never pushes the documentation commit directly to `main`. If `main` targets a different product version, the workflow synchronizes only its release workspace and logs a warning. Historical release numbers outside the marked current-source references are intentionally preserved.
