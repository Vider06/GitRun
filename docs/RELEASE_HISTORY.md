# Release history and historical verification

Current source target: GitRun <!-- gitrun-version:start -->1.8.9<!-- gitrun-version:end --> (`version.txt`). Current-source markers are synchronized by `scripts/sync-markdown-version.py`; entries below record historical source changes and do not by themselves prove that signed release artifacts were published.

## GitRun 1.8.9 — Main Library recovery and protected JSON diagnostics

- **Source/product target:** `1.8.9`, recorded in `version.txt`. This entry describes the source line; it does not claim that a signed release artifact has been published.
- Run the signed official Main Library restore in a separate transient systemd unit rather than inheriting the Control Manager's read-only mount namespace. The transient unit keeps `ProtectSystem=full` and makes only `/usr/share/gitrun/library` writable.
- Keep the restore on fixed, root-owned GitRun and systemd-run executables; the UI cannot supply arbitrary paths or commands.
- Make `gitrun doctor` explicitly report protected JSON paths skipped because of expected non-root permissions, while malformed JSON and unexpected permission failures remain warnings or failures. Run `sudo gitrun doctor` when full validation of root-owned files is required.
- Route `gitrun update --check` through a dedicated read-only dispatcher, separate from the transaction that downloads and installs an update; add parser regression coverage for the flag.
- Bump all workspace crate patch versions and synchronize `Cargo.lock`, `runner-bootstrap.lock` and the current-source Markdown markers.
- **Implementation PR:** [#135](https://github.com/Vider06/GitRun/pull/135).

## GitRun 1.8.8 — CI cache fingerprints and version alignment

- **Source/product target:** `1.8.8`, recorded in `version.txt`. This entry describes the source line; it does not claim that a signed release artifact has been published.
- Replaced GitHub Actions `hashFiles()` cache-key evaluation with `scripts/ci/hash-tracked-files.sh`, which fingerprints tracked paths and contents after checkout. CI/release cache consumers reuse the computed digest; existing restore prefixes remain available.
- Kept workflow job timeouts unchanged and made both the cache helper and runtime-contract test script executable in Git.
- Synchronized `version.txt`, the workspace crate patch versions, `Cargo.lock` and `runner-bootstrap.lock`.
- **Implementation PR:** [#133](https://github.com/Vider06/GitRun/pull/133).

## GitRun 1.8.7 — JSON recovery, Main Library integrity and CPU history

- **Source/product target:** `1.8.7`, recorded in `version.txt`. This entry describes the source line; it does not by itself claim that a signed release artifact is published.
- Expanded `gitrun doctor` JSON diagnostics so each discovered file is checked and the report distinguishes malformed JSON, inaccessible files and safety-limit failures while preserving terminal scrollback.
- Added conservative JSON repair, explicit restoration from sibling `.json.Back` backups, and an explicit Main Library restore operation. Repair normalizes only recognized safe cases; ambiguous input is left untouched. Recovery writes are atomic and reject symbolic-link targets.
- Added a separate `library.zip` release artifact containing the 11 selected JSON reference files as `.json.Back` entries. The signed release manifest supplies its checksum; validation checks archive membership, safe paths, JSON shape and extraction limits. Setup/updater populate missing Main Library entries and create only missing local backups; they do not overwrite existing copies during routine runs. The explicit Main Library restore changes the reference-library area, not active operational configuration.
- Added actual host CPU utilization history to the dashboard Overview and a CPU sparkline in `gitrun manage` when the backend supplies samples. The history is bounded to 60 samples, is host-wide rather than per-job telemetry, and is process-local so it resets when GitRun restarts.
- Protected system configuration reads can use the existing fixed-purpose Control Manager path after a real permission-denied result; inaccessible protected state is not treated as corruption and permissions are not loosened as a fallback.
- Corrected the release workflow's duplicate-library-member check so it counts archive names line-by-line instead of falsely detecting duplicates due to an incorrectly escaped `printf` format.
- **Implementation PRs:** [#129](https://github.com/Vider06/GitRun/pull/129) and [#131](https://github.com/Vider06/GitRun/pull/131).

## GitRun 1.8.6 — runner-network bootstrap and Logs clipboard shortcut

- **Source/product target:** `1.8.6`, recorded in `version.txt` for this historical source line.
- Fixed managed runner-network bootstrap when Docker reports the exact named-network error `network gitrun-runner not found`. The scheduler can reach the existing creation path for the configured missing network while unrelated network names and permission failures remain errors; the GitRun-managed bridge and Linux inter-container isolation checks remain in place.
- Added a Logs-only `c Copy logs` shortcut in `gitrun manage`. It sends an OSC 52 clipboard-write request for loaded log output, bounded to the newest 64 KiB; acceptance depends on the local terminal and any SSH/tmux configuration. GitRun does not read clipboard contents or claim that the request was accepted.
- Updated `gitrun-scheduler` from 0.11.1 to 0.11.2 and `gitrun-cli` from 0.13.6 to 0.13.7 for the corresponding source changes.
- **Implementation PR:** [#127](https://github.com/Vider06/GitRun/pull/127).

## GitRun 1.8.5 — responsive management-console polish

- **Source/product target:** `1.8.5`, recorded in `version.txt` for this historical source line.
- Replaced the Settings gear with the neutral `≡` glyph, added restrained TUI panel styling, and introduced a compact cat status panel only when the terminal is at least 110 columns by 24 rows. The panel reuses validated sprite data and reports UI operation state; it is not an independent health check.
- Added structural Markdown-marker validation so ordinary PR checks can verify marker pairing/coverage without requiring version values to match early. Strict value synchronization remains part of release preparation.
- Follow-up release automation now opens or refreshes a docs-only PR for marker synchronization instead of pushing documentation directly to protected `main`.
- Updated `gitrun-cli` from 0.13.5 to 0.13.6 for the terminal-console changes.
- **Implementation PRs:** [#124](https://github.com/Vider06/GitRun/pull/124) and [#125](https://github.com/Vider06/GitRun/pull/125).

## GitRun 1.8.4 — crate version reconciliation and Markdown automation

- **Source/product target:** `1.8.4`, recorded in `version.txt`. This entry records source changes; it does not by itself assert that a signed release artifact has been published.
- Reconciled independent patch versions for crates with source changes after their previous version bumps: `gitrun-cli` 0.13.4 → 0.13.5, `gitrun-updater` 0.8.3 → 0.8.4, `gitrun-recovery` 0.6.7 → 0.6.8, `gitrun-setup` 0.9.4 → 0.9.5, `gitrun-scheduler` 0.11.0 → 0.11.1, and `gitrun-smart-scheduler` 0.4.0 → 0.4.1. Crates whose code did not change retain their versions.
- Added invisible Markdown version markers and a standard-library-only synchronizer. PR CI validates marker pairing and required coverage; release packaging refreshes marker values and runs the strict version check. When `main/version.txt` matches the selected release, the workflow opens or refreshes a docs-only PR instead of pushing directly to `main`.
- Synced the Tauri bundle, frontend package and npm lockfile product metadata to 1.8.4, with the release workflow deriving those values from the selected product version.
- Polished the terminal management console: requested text presentation for the Settings gear, width-aware header, restrained selected-row tint and semantic JSON output colors. The variation selector does not force text rendering on every terminal/font combination; the follow-up TUI polish replaces that icon with a neutral text glyph.
- **Implementation PR:** [#123](https://github.com/Vider06/GitRun/pull/123).

## GitRun 1.8.3 — updater resilience and explicit force mode

- **Source/product target:** `1.8.3`, recorded in `version.txt` for this source line. This entry records source changes; it does not by itself assert that a signed release artifact has been published.
- Replaced latest-release discovery through the unauthenticated GitHub REST API with the HTTPS release-manifest download route, while preserving manifest signature verification and artifact SHA-256 verification.
- Added a privileged-update preflight that verifies the release and checks platform applicability before prompting for sudo; the privileged retry is pinned to the verified manifest version.
- Added `gitrun update --force` to skip only the post-update doctor gate when an operator has a known false negative. Signed-manifest, SHA-256, executable, systemd-reconciliation and scheduler-restart checks remain active, and failures in those stages can still fail or roll back an update.
- Made the global `--no-cat` flag compatible with both `gitrun --no-cat doctor` and the legacy `gitrun doctor --no-cat` invocation used by older updaters during replacement.
- Hardened CI tool downloads and retry status handling; the pinned Cargo Audit version now has a source-install fallback when its prebuilt release asset cannot be downloaded.
- **Implementation PR:** [#122](https://github.com/Vider06/GitRun/pull/122).

## GitRun 1.8.2 — system recovery and local runner-image fixes

- **Implementation PRs:** [#120](https://github.com/Vider06/GitRun/pull/120) and [#121](https://github.com/Vider06/GitRun/pull/121).
- Improved health and terminal-setup behavior, made recovery diagnostics account for the persistent Control Manager service, and reconciled missing or stale GitRun service units through the existing repair path.
- Fixed permanent-runner updates for unqualified local image names such as `gitrun-runner:latest`: they are verified locally rather than accidentally treated as Docker Hub images. Registry-qualified references continue through the pull path.
- Refreshed the management console layout and current-operation details, including resource gauges, focus/navigation behavior and selection styling. PR #123 further polishes the Settings glyph, header sizing and JSON output.

## GitRun 1.8.1 — update repair and service recovery

- **Implementation PR:** [#119](https://github.com/Vider06/GitRun/pull/119).
- Added `gitrun fix-update` to repair the installed systemd integration and `gitrun-control` group, then apply the latest official signed release only when it is newer than the installed binary.
- Kept update recovery bounded: the command does not force a downgrade, and service/update work remains subject to the updater's verification and rollback contracts.

## GitRun 1.8.0 — CLI management console and version alignment

- **Source/product version:** `1.8.0`, recorded in `version.txt` and the dashboard product metadata. This entry describes the source line; it does not assert that a signed release artifact has been published.
- Added the keyboard-driven `gitrun manage` terminal management console for headless operation, using the dashboard's validated backend operations rather than a parallel settings implementation.
- Added the persistent privileged Control Manager service and typed, allowlisted local IPC for protected dashboard/terminal operations. The UI remains unprivileged; fixed-purpose reads/writes and explicit confirmation gates replace repeated broad sudo interactions. Setup and update flows install or reconcile the service unit.
- Standard CLI invocations show the mascot by default; `--no-cat` is the explicit opt-out. Internal dashboard/recovery child-process launches pass `--no-cat` so their output remains usable and does not interfere with parent terminal rendering.
- Hardened CLI argument-error/help handling around the shared presenter and serialized Linux process-control regression tests to avoid unrelated-child `waitpid` races.
- Bumped patch versions only for crates changed by this work: `gitrun-cli` 0.13.2 → 0.13.3, `gitrun-dashboard-tauri` 0.11.0 → 0.11.1, `gitrun-gsr` 0.6.2 → 0.6.3 and `gitrun-recovery` 0.6.6 → 0.6.7. Other crates retain their independent SemVer versions.


## GitRun 1.7.0 — Kitty queue-aware evaluation primitives

- **Implementation PR:** [#110](https://github.com/Vider06/GitRun/pull/110). The feature's introduction remains a 1.7.0 historical milestone.
- Added dependency-free Erlang-C M/M/c queue-wait estimation, deterministic bounded FCFS replay metrics and a capped runner-floor evaluation helper in `gitrun-smart-scheduler::queueing`.
- The new helpers are evaluation primitives, not a live MPC controller. Production scaling remains behind the existing deterministic reconciliation and resource/policy gates. Erlang-C assumptions and future-data leakage are explicitly documented.
- This source history entry does not imply a published signed 1.7.0 artifact; use GitHub Releases for installable artifacts.


This page keeps the major historical milestones in one place. It is **not** a release download page and does not imply that every source version listed here was published as a signed binary. Use [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts and inspect CI for the exact commit you intend to deploy.

## GitRun 1.5.0 — core refactor baseline

- **PR:** [#83](https://github.com/Vider06/GitRun/pull/83), merged 2026-10-19.
- **Audited source HEAD:** [`5afbe14`](https://github.com/Vider06/GitRun/commit/5afbe14ef3144b8e27770e2058943eff8614bc7c).
- The refactor established the Rust-native runtime, dashboard baseline, deterministic scheduler, VM resolution, GitDockRun registry checks, GSR watchdog/security hardening and optional Kitty Smart Scheduler.
- GitRun CI, CodeQL, DevSkim, Rustfmt and Cargo Lock Maintenance passed on the audited source HEAD. These results apply only to that historical commit; they do not certify later changes or prove every deployment-specific lifecycle was tested end to end.

## GitRun 1.5.1 — protected-state and legacy-install recovery

- **PR:** [#103](https://github.com/Vider06/GitRun/pull/103), merged 2026-10-20.
- Recovery distinguishes root-only protected records from missing or corrupt state. A permission-denied read of setup, health or crash records is not itself evidence of corruption.
- The legacy migration is intentionally narrow: it validates trusted configuration and systemd paths, rejects unexpected owners and links, avoids recursive ownership changes, preserves protected records and restores the packaged `gitrun.service` and `gitrun-gsr.service` contracts.
- The product version lives in `version.txt` and dashboard package metadata; Cargo crates keep independent SemVer versions.

For current recovery behavior, follow [Security](../SECURITY.md), the [security model](SECURITY_MODEL.md) and the [systemd unit notes](../systemd/README.md), rather than treating this historical summary as a repair runbook.

## GitRun 1.6.1 — runtime and package lifecycle fixes

See [v1.6.1 fixes](v1.6.1-fixes.md) for the detailed patch record covering protected dashboard reads, scheduler recovery, GitHub permission diagnostics and non-destructive package removal.

## GitRun 1.6.2 — protected dashboard settings

See [v1.6.2 fixes](v1.6.2-fixes.md) for the shared settings loader and narrowly scoped administrator-authorized read used only after a real permission-denied error.

## Verification rule

A green workflow is evidence only for the jobs and commit it actually checked. It does not prove that no open security alerts remain, that privileged desktop authorization works on every supported machine, that VM provisioning/recovery was tested on a real hypervisor, or that a customized runner image migrates safely. Validate those flows in the target environment before production rollout.


## 1.7.0 source-line additions — Future Mapping and Think

- Added optional Future Mapping: a private, revisioned one-hour map with twelve five-minute demand buckets and CPU/RAM envelopes bounded by operator settings.
- Added optional Think: comparison of independent lookback windows that holds predictive growth when sample coverage is incomplete or estimates disagree.
- Added revision-bound, 60-second typed patches; GSR must authorize the exact current map revision before predictive capacity changes. The deterministic scheduler and shared host budget remain authoritative.
- Improved the Erlang-C implementation to use a stable Erlang-B recurrence for large runner pools, with a regression test at the configured model bound.
- Bumped the smart-scheduler crate to 0.4.0 and the scheduler/dashboard crates to 0.11.0. `version.txt` remains 1.7.0 for the product source line.

These capabilities are experimental and disabled by default. This history does not assert real-world forecast calibration or that all environment-specific release gates have passed.
