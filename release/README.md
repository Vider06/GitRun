# Release metadata

This directory contains machine-readable release manifests and schemas used to describe GitRun artifacts and validate manifest structure. The source version is maintained in the root `version.txt`; individual Cargo crates keep their own SemVer versions.

Official release manifests are signed by the release workflow. Signature verification is required for official releases. Unsigned custom manifests need an explicit compatibility/unsafe opt-out.

PR [#83](https://github.com/Vider06/GitRun/pull/83) established the 1.5.0 source baseline on 2026-10-19. PR [#103](https://github.com/Vider06/GitRun/pull/103) then merged the protected-state diagnostics and guarded legacy runtime/systemd repair on 2026-10-20. This branch proposes product/source version 1.5.1; the root `version.txt` and dashboard metadata carry the product version, while Cargo crates keep independent SemVer versions (including `gitrun-setup` 0.9.1 and `gitrun-recovery` 0.6.4). A merged PR or version bump is not proof that a signed release tag/binary has been published; check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts. See [the 1.5.1 release notes](../docs/v1.5.1-release-notes.md).
