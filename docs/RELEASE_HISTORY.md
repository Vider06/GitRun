# Release history and historical verification

## GitRun 1.8.0 — CLI management console and version alignment

- **Source/product version:** `1.8.0`, recorded in `version.txt` and the dashboard product metadata. This entry describes the source line; it does not assert that a signed release artifact has been published.
- Added the keyboard-driven `gitrun manage` terminal management console for headless operation, using the dashboard's validated backend operations rather than a parallel settings implementation.
- Standard CLI invocations show the mascot by default; `--no-cat` is the explicit opt-out. Internal dashboard/recovery child-process launches pass `--no-cat` so their output remains usable and does not interfere with parent terminal rendering.
- Hardened CLI argument-error/help handling around the shared presenter and serialized Linux process-control regression tests to avoid unrelated-child `waitpid` races.
- Bumped patch versions only for crates changed by this work: `gitrun-cli` 0.13.2 → 0.13.3, `gitrun-dashboard-tauri` 0.11.0 → 0.11.1, `gitrun-gsr` 0.6.2 → 0.6.3 and `gitrun-recovery` 0.6.6 → 0.6.7. Other crates retain their independent SemVer versions.


## GitRun 1.7.0 — Kitty queue-aware evaluation primitives

- **Working branch:** `feat/kitty-queue-aware-control-v1.7.0` (pull request checks must verify the final commit).
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
