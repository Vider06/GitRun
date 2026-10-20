# `gitrun-updater`

GitRun update engine. It resolves releases, verifies downloaded artifacts, preserves runtime state, performs rollback-safe installation and coordinates runner-image updates.

The PR #83 source-head regression suite passed signature verification, tampering rejection, malicious archive-path rejection and rollback after a failed post-install health check. Confirm the release signing configuration and rollback behavior on the target installation before production rollout; see the [status report](../../docs/v1.5.0-refactor-status.md).
