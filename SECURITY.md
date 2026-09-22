# Security

GitRun controls Docker containers and GitHub Actions self-hosted runners. A GitRun host is trusted infrastructure.

## Threat model

A GitHub Actions workflow executes repository-controlled code inside its assigned runner. GitRun-managed runners intentionally mount the host Docker Engine socket because CI jobs may need Docker builds and container operations.

That socket is a major security boundary. Code running in a managed runner can potentially use Docker to control the host, access host-mounted data, or affect other containers. GitRun should therefore be used only with repositories and workflow code that the operator trusts.

GitRun is not intended to be an anonymous public runner service.

## Credential handling

- Never commit GITHUB_TOKEN, runner registration tokens, passwords, private keys, or runtime environment files.
- Keep runtime environment files private and protect them with restrictive filesystem permissions.
- Use a dedicated GitHub credential with only the repository and Actions permissions required by the deployment.
- Registration tokens are generated on demand and should not be stored.
- Rotate a credential immediately if it appears in source control, logs, CI output, backups, or process inspection.

## Container hardening

Managed runner containers use:

- a read-only container root filesystem;
- a private writable /tmp;
- a dedicated writable Actions runner work directory;
- CPU, memory, and PID limits;
- a shared Docker volume for selected build caches.

The runner work directory must remain writable because the GitHub Actions runner stores configuration, diagnostics, and job state there.

These controls reduce accidental writes and resource abuse but do not make a runner containing the host Docker socket a strong security sandbox.

## Host security

- Do not expose the Docker API directly to the Internet.
- Keep the GitRun host patched.
- Restrict local administrative access.
- Connect only repositories appropriate for the host's trust boundary.
- Prefer ephemeral runners when the workload benefits from short-lived runner state.
- Review changes to Docker mounts, privileges, namespaces, and systemd controls as security-sensitive changes.

## Reporting a vulnerability

Please report security vulnerabilities privately through GitHub's security advisory mechanism:

https://github.com/Vider06/GitRun/security/advisories/new

Do not publish active exploit details in a public issue. Include enough information to reproduce and assess the problem safely.

Security fixes may require coordinated disclosure when public release timing could materially increase risk.
