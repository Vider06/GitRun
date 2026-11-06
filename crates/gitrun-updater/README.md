# `gitrun-updater`

GitRun update engine. It resolves releases, verifies downloaded artifacts, preserves runtime state, performs rollback-safe installation and coordinates runner-image updates.

The PR #83 source-head regression suite passed signature verification, tampering rejection, malicious archive-path rejection and rollback after a failed post-install health check; those results are scoped to the historical 1.5.0 source. The product version is maintained in `version.txt`, the bundled dashboard metadata, and release manifests; individual Cargo crates retain independent SemVer. Confirm release-signing configuration and rollback behavior on the target installation before rollout. See the [consolidated release history](../../docs/RELEASE_HISTORY.md).
## Classified release selection

From GitRun 1.9.1 onward, the signed release manifest carries a `release_type` (`pre`, `full` or `security`). The updater cross-checks it against the GitHub release description marker and `prerelease` flag, paginates the Releases API, applies the selected Security/Full/Pre policy, and uses SemVer ordering without downgrades. Host-incompatible artifacts are skipped in favor of an older eligible release when one exists; invalid signatures or mismatched metadata fail closed.

The policy and diagnostics persistence belong to `gitrun-management`, not this engine crate. The GUI, TUI and CLI share that backend. See the [Updater Release Policy](../../docs/UPDATER_RELEASE_POLICY.md).
