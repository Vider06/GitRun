# `gitrun-core`

Shared GitRun domain and policy layer. It owns configuration, GitHub authentication, runner models, state, workflow validation, compatibility analysis, API policy and the shared VM decision protocol.

Settings persistence includes a stable sidecar advisory lock used by supported GitRun writers and security-sensitive policy operations. This coordinates the official writer paths; a privileged administrator who directly edits/replaces the policy file while bypassing the lock remains outside that contract. See the [security model](../../docs/SECURITY_MODEL.md), [consolidated release history](../../docs/RELEASE_HISTORY.md), [consolidated release history](../../docs/RELEASE_HISTORY.md).
