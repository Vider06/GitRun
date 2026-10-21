# `gitrun-core`

Shared GitRun domain and policy layer. It owns configuration, GitHub authentication, runner models, state, workflow validation, compatibility analysis, API policy and the shared VM decision protocol.

Settings persistence includes a stable sidecar advisory lock used by supported GitRun writers and security-sensitive policy operations. This coordinates the official writer paths; a privileged administrator who directly edits/replaces the policy file while bypassing the lock remains outside that contract. See the [security model](../../docs/SECURITY_MODEL.md), [1.6.2 protected-settings fix](../../docs/v1.6.2-fixes.md), [1.5.1 recovery notes](../../docs/v1.5.1-release-notes.md), and [historical 1.5.0 status tracker](../../docs/v1.5.0-refactor-status.md).
