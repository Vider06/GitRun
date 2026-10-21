# Release metadata

PR [#83](https://github.com/Vider06/GitRun/pull/83) established the 1.5.0 source baseline on 2026-10-19. PR [#103](https://github.com/Vider06/GitRun/pull/103) added protected-state diagnostics and guarded legacy runtime/systemd repair on 2026-10-20. The current 1.6.2 source line includes follow-up dashboard recovery fixes in PR [#109](https://github.com/Vider06/GitRun/pull/109).

The root `version.txt` and dashboard product metadata carry the product/source version. Cargo crates retain independent SemVer versions; bump each affected crate according to compatibility and dependency constraints rather than assuming all crate versions equal the product version. Keep both lockfiles aligned with workspace package metadata.

A merged pull request, a passing CI run, or a `version.txt` bump is not proof that a signed release tag or binary has been published. Check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts, and validate release-signing configuration and rollback behavior on the target installation before rollout.

See the [1.6.2 fixes](../docs/v1.6.2-fixes.md), [1.6.1 fixes](../docs/v1.6.1-fixes.md), and [historical 1.5.1 recovery notes](../docs/v1.5.1-release-notes.md).
