# `gitrun-vault`

GitVault provides encrypted-at-rest secret storage for GitRun. Secret values use AES-256-GCM and support global, group and repository scopes; more-specific scopes override broader ones.

The Vault is a separate trust boundary: runner-facing APIs must resolve the caller's authorized repository and scope before accessing a secret. Never store plaintext credentials in configuration examples, logs or committed fixtures. See the [security model](../../docs/SECURITY_MODEL.md) and [workflow API reference](../../docs/GITRUN_API.md).
