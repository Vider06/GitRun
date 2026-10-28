# Release metadata

PR [#83](https://github.com/Vider06/GitRun/pull/83) established the 1.5.0 source baseline on 2026-10-19. PR [#103](https://github.com/Vider06/GitRun/pull/103) added protected-state diagnostics and guarded legacy runtime/systemd repair on 2026-10-20. The current <!-- gitrun-version:start -->1.8.15<!-- gitrun-version:end --> source target fingerprints local runner build inputs, builds into a temporary image tag, verifies the expected original-Bash entrypoint and runtime tmpfs policy bootstrap, promotes only validated images, and triggers GTUU reconciliation. Registry-qualified images retain signed-manifest digest verification; busy permanent runners remain untouched. This builds on the 1.8.14 GSR bootstrap ordering fix and earlier recovery timeout, API socket safety, Docker seccomp and Main Library diagnostics. It builds on the CI cache-fingerprint helper from PR [#133](https://github.com/Vider06/GitRun/pull/133) and the signed Main Library restore / protected JSON diagnostic fixes from PR [#135](https://github.com/Vider06/GitRun/pull/135), plus the earlier runner-network bootstrap correction, JSON recovery, bounded host CPU history, updater resilience, TUI/version-marker improvements, and Kitty queue-aware evaluation, Future Mapping and Think. See the [consolidated release history](../docs/RELEASE_HISTORY.md) for source-line boundaries and verification limits.

The root `version.txt` is the product-version source of truth. The release workflow applies it to marked current-version references in Markdown, the Tauri bundle configuration, frontend `package.json` and `package-lock.json`, then runs a strict check on the synchronized release workspace. PR CI checks marker pairing and required coverage without prematurely requiring value equality. If `main/version.txt` matches the selected release, the workflow publishes the Markdown-only correction on a dedicated automation branch and opens or refreshes a pull request; it never pushes directly to `main`. Cargo crates retain independent SemVer versions and only crates with source changes receive bumps. Keep `Cargo.lock` and the frontend package lock aligned with their corresponding manifests.

## Main Library reference artifact

The release workflow publishes `library.zip` as the `library-json-backup` target in the signed `release-manifest.json`. The archive contains 11 selected JSON reference inputs, each stored with a `.json.Back` suffix:

- `Core/Dockers/runners/linux-x86_64/profiles.json.Back`
- `crates/gitrun-cli/src/cat/cat_states.json.Back`
- `crates/gitrun-cli/src/cat/gitrun.json.Back`
- `crates/gitrun-dashboard-tauri/package.json.Back`
- `crates/gitrun-dashboard-tauri/package-lock.json.Back`
- `crates/gitrun-dashboard-tauri/src-tauri/capabilities/default.json.Back`
- `crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json.Back`
- `crates/gitrun-recovery/capabilities/default.json.Back`
- `crates/gitrun-recovery/tauri.conf.json.Back`
- `release/release-manifest.example.json.Back`
- `release/release-manifest.schema.json.Back`

The release check requires 11 distinct, safe member paths and valid JSON object/array content. The updater/recovery path verifies the signed manifest and the archive checksum, then enforces archive path and expansion limits before restoration. Routine setup/update fills missing reference-library entries and creates only missing local backups; existing entries are not overwritten during those routine flows. The explicit **Restore Main Library** action restores the dedicated reference library and does not overwrite active operator configuration.

A merged pull request, a passing CI run, or a `version.txt` bump is not proof that a signed release tag or binary has been published. Check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts, and validate release-signing configuration and rollback behavior on the target installation before rollout.

See the [Kitty Smart Scheduler guide](../docs/SMART_SCHEDULER.md), the historical [1.6.2 fixes](../docs/v1.6.2-fixes.md), [1.6.1 fixes](../docs/v1.6.1-fixes.md), and [consolidated release history](../docs/RELEASE_HISTORY.md).


Future Mapping and Think are disabled by default and remain advisory. Capacity changes still pass the scheduler's deterministic floor, resource-pressure and adaptive host-budget checks. The scheduler also rejects a GSR permit if the map revision has changed or expired before use. These controls do not establish calibrated forecast accuracy; review the Kitty guide and test on the target host before deployment.


The 1.8.0 source line introduced the keyboard-driven `gitrun manage` terminal console and consistent CLI mascot suppression for internal child-process launches. See [the release history](../docs/RELEASE_HISTORY.md) for scope and versioning boundaries.
