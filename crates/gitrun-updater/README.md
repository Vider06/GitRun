# `gitrun-updater`

GitRun update engine. It resolves releases, verifies downloaded artifacts, preserves runtime state, performs rollback-safe installation and coordinates runner-image updates.

The PR #83 source-head regression suite passed signature verification, tampering rejection, malicious archive-path rejection and rollback after a failed post-install health check; those results are scoped to the historical 1.5.0 source. The product version is maintained in `version.txt`, the bundled dashboard metadata, and release manifests; individual Cargo crates retain independent SemVer. Confirm release-signing configuration and rollback behavior on the target installation before rollout. See the [consolidated release history](../../docs/RELEASE_HISTORY.md).
