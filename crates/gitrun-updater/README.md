# `gitrun-updater`

GitRun update engine. It resolves releases, verifies downloaded artifacts, preserves runtime state, performs rollback-safe installation and coordinates runner-image updates.

The v1.5.0 regression suite covers fail-closed signature verification, tampering rejection, malicious archive-path rejection and restoration of the previous installation/state when a post-install health check fails. These tests do not replace validating release signing configuration and update recovery on the target installation environment. See [the updater/security review tracker](../../docs/v1.5.0-refactor-status.md).
