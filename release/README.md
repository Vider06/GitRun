# Release metadata

This directory contains machine-readable release manifests and schemas used to describe GitRun artifacts and validate manifest structure. The source version is maintained in the root `version.txt`; individual Cargo crates keep their own SemVer versions.

Official release manifests are signed by the release workflow. Signature verification is required for official releases. Unsigned custom manifests need an explicit compatibility/unsafe opt-out.

PR [#83](https://github.com/Vider06/GitRun/pull/83) merged the GitRun 1.5.0 source refactor to `main` on 2026-10-19. A merged PR and source version are not proof that a release tag or binary has been published; check [GitHub Releases](https://github.com/Vider06/GitRun/releases) for published artifacts.
