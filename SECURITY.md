# Security

GitRun controls Docker containers and GitHub Actions self-hosted runners. A GitRun host is trusted infrastructure.

## Threat model

A GitHub Actions runner executes repository-controlled workflow code. A compromised or malicious workflow can therefore access everything available inside its runner container and, on the current Linux implementation, potentially control the Docker host through the mounted Docker socket. Every GitRun-managed Linux runner currently receives that socket, so connected repositories must be treated as trusted infrastructure. A per-repository Docker-socket opt-in is not yet implemented.

The manager requires Docker socket access because it creates and removes runner containers. Keep the manager isolated, do not expose Docker's API publicly, and restrict host access.

Every GitRun-managed Linux runner currently receives `/var/run/docker.sock`. This is a high-risk privileged mode: code running inside such a runner can use the Docker API available through the mounted socket. GSR container hardening reduces other container and kernel attack surfaces, but it does not remove the privileges exposed by the Docker socket.

## Credential handling

- Never commit `GITHUB_TOKEN` or runtime environment files.
- Keep `/etc/gitrun/gitrun.env` mode 0600.
- Use a dedicated GitHub credential with only the repository/Actions permissions required by the installation. GitHub App authentication is supported as an alternative to PAT authentication; protect the App private key as a high-value credential.
- Registration tokens are short-lived and should never be persisted.
- Prefer ephemeral runners for untrusted or isolation-sensitive workloads.
- Runner tokens are unset in the runner process environment after configuration.
- Rotate credentials if they are exposed in logs, process inspection, backups, or source control.

## Container hardening

Linux runner containers use CPU, memory and PID limits. The current Linux runner root filesystem remains writable so normal GitHub Actions jobs can install tools and packages. Isolation is provided by the dedicated runner home backend (tmpfs by default, or a per-runner Docker volume), a 256 MiB `/tmp` tmpfs, the GSR capability/no-new-privileges hardening, and the dedicated `/run/gitrun` tmpfs.

The Docker socket is not equivalent to an ordinary container mount. Because the current Linux implementation mounts it into every managed runner, treat every connected repository as trusted to exercise Docker host authority. Verify repository trust before adding it to a runner pool.

Runner containers should not otherwise receive privileged mode, host networking, host PID/IPC namespaces, or arbitrary host filesystem mounts. The Docker socket itself remains the intentional privileged compatibility boundary for Linux runners.

The manager itself necessarily has access to the Docker socket and must therefore be treated as a host-administrator component.

## Network and GitHub

- Runners need outbound HTTPS access to GitHub.
- Do not expose the Docker daemon or GitRun control interfaces to the public Internet.
- Pin or regularly review runner image versions and base images.
- Review every repository before connecting it to a runner pool.

## Reporting

Report security issues privately through GitHub's security reporting mechanism rather than publishing exploit details in a public issue.

## Operational checklist

Before production:
1. Create a dedicated least-privilege GitHub credential.
2. Restrict access to the GitRun host.
3. Enable ephemeral runners for untrusted workloads.
4. Treat every connected Linux repository as trusted to use Docker host authority through the mounted socket; verify that it does not require additional privileged flags, host namespaces, or arbitrary host mounts.
5. Keep Docker, the runner image, GitRun and the host OS patched.
6. Test recovery and credential rotation.

GitRun is intended for administrators managing repositories they trust. It is not an open public runner service.


## GLib / RUSTSEC-2024-0429

GitRun's Linux Tauri 2 stack currently resolves the GTK3 bindings to `glib 0.18.5`. The advisory affects that release, while the published fix is `glib >= 0.20.0`; moving the existing Tauri 2 GTK3 stack to that API level is not a compatible point update. Tauri's GTK4/WebKitGTK 6 migration is part of the Tauri 3 line, which is not used here.

GitRun therefore pins the byte-identical backport of the upstream `VariantStrIter::impl_get` fix from gtk-rs/gtk-rs-core PR #2009 at commit `ea720152f28e293ef4362ee844ee5cc499f32d2a`. The pin is immutable. The RustSec advisory is allowed in `cargo audit` only because the scanner keys on the package version (`0.18.5`) and cannot represent this source-level backport; this is not an acceptance of the vulnerable implementation.

Revisit and remove this pin as soon as either an official `glib 0.18.6` containing the backport is released or GitRun migrates to a stable GTK4/WebKitGTK 6 stack.


## Git*Run API and local IPC

The workflow-facing Git*Run API is a closed set of explicit operations transported over the local Unix socket at `/run/gitrun/api.sock` by default. The socket path may be changed with `GITRUN_API_SOCKET`. The API launcher does not execute privileged work itself: it sends a structured request containing GitHub workflow identity to the host-side authorization boundary.

GSR validates the API/operation contract, effective repository policy, resource restrictions and the existing command policy before an authorized operation reaches `gitrun-exe`. The private GSR-to-executor envelope uses HMAC-SHA256, random nonces, timestamp validation and replay protection.

The API socket is therefore an authorization boundary, not a sandbox. A Linux runner that can already use the host Docker socket retains Docker host authority. Protect the socket filesystem location and keep GitRun's runtime state directory accessible only to the service/dashboard accounts that require it.

## VM-backed runners

VM-backed Logic Containers can route jobs to Docker daemons inside KVM/libvirt or VirtualBox guests. Protect the guest Docker endpoint and base disk image as trusted infrastructure. VM configuration does not remove the need to review repository trust, workflow permissions or guest credentials.
