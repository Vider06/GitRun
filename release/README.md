# Release metadata

PR [#83](https://github.com/Vider06/GitRun/pull/83) established the 1.5.0 source baseline on 2026-10-19. PR [#103](https://github.com/Vider06/GitRun/pull/103) added protected-state diagnostics and guarded legacy runtime/systemd repair on 2026-10-20. The current 1.8.0 source line extends PR [#110](https://github.com/Vider06/GitRun/pull/110) with Kitty queue-aware evaluation, bounded Future Mapping and Think; it also includes the dashboard recovery fixes from PR [#109](https://github.com/Vider06/GitRun/pull/109).

The root `version.txt` and dashboard product metadata carry the product/source version. Cargo crates retain independent SemVer versions; bump each affected crate according to compatibility and dependency constraints rather than assuming all crate versions equal the product version. Keep both lockfiles aligned with workspace package metadata.

A merged pull request, a passing CI run, or a `version.txt` bump is not proof that a signed release tag or binary has been published. Check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts, and validate release-signing configuration and rollback behavior on the target installation before rollout.

See the [Kitty Smart Scheduler guide](../docs/SMART_SCHEDULER.md), the historical [1.6.2 fixes](../docs/v1.6.2-fixes.md), [1.6.1 fixes](../docs/v1.6.1-fixes.md), and [consolidated release history](../docs/RELEASE_HISTORY.md).


Future Mapping and Think are disabled by default and remain advisory. Capacity changes still pass the scheduler's deterministic floor, resource-pressure and adaptive host-budget checks. The scheduler also rejects a GSR permit if the map revision has changed or expired before use. These controls do not establish calibrated forecast accuracy; review the Kitty guide and test on the target host before deployment.


The 1.8.0 source line also includes the keyboard-driven `gitrun manage` terminal console and consistent CLI mascot suppression for internal child-process launches. See [the release history](../docs/RELEASE_HISTORY.md) for scope and versioning boundaries.
