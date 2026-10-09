# Security

GitRun controls Docker containers and GitHub Actions self-hosted runners. A GitRun host is trusted infrastructure.

## Threat model

A GitHub Actions runner executes repository-controlled workflow code. A compromised or malicious workflow can therefore access everything available inside its runner container. On Linux, a workflow can additionally control the Docker host only when that repository's Docker compatibility policy explicitly enables direct Docker-socket access.

The manager requires Docker socket access because it creates and removes runner containers. Keep the manager isolated, do not expose Docker's API publicly, and restrict host access.

Direct `/var/run/docker.sock` access is **disabled by default** for GitRun-managed workflow runners. It is a per-repository compatibility opt-in (`DockerPolicy::direct_socket_enabled`) and should only be enabled for repositories whose workflows genuinely require direct Docker daemon access. When enabled, the runner receives the Docker socket and its corresponding socket group, giving workflow code Docker host authority. GSR container hardening reduces other container and kernel attack surfaces, but it does not remove the privileges exposed by the Docker socket.

## Credential handling

- Never commit `GITHUB_TOKEN` or runtime environment files.
- Keep `/etc/gitrun/gitrun.env` mode 0600.
- Use a dedicated GitHub credential with only the repository/Actions permissions required by the installation. GitHub App authentication is supported as an alternative to PAT authentication; protect the App private key as a high-value credential.
- Registration tokens are short-lived and should never be persisted.
- Prefer ephemeral runners for untrusted or isolation-sensitive workloads.
- Runner tokens are unset in the runner process environment after configuration.
- Rotate credentials if they are exposed in logs, process inspection, backups, or source control.
- Treat the GitHub Actions release-signing private key as a high-value credential. Keep it only in `GITRUN_UPDATE_SIGNING_PRIVATE_KEY_B64`. The official GitRun CLI includes the matching public verification key and key ID as trusted defaults; custom deployments may explicitly override the public-key settings. Never distribute or commit the private key.
- Rotate the release-signing key by publishing a new trusted public key/key identifier before retiring the previous signing key; never commit the private key.

## Container hardening

Linux runner containers use CPU, memory and PID limits and a read-only root filesystem by default. Writable state is explicitly mounted through a per-runner Docker volume at the Actions runner home, a 256 MiB `/tmp` tmpfs, the shared cache volume, and the dedicated `/run/gitrun` tmpfs. GSR capability/no-new-privileges hardening remains in place. Workflows that need to mutate the base operating-system filesystem should use a prepared runner image; disabling the read-only root requires the explicit unsafe-runner gate.

The Docker socket is not equivalent to an ordinary container mount. Because direct socket access is **disabled by default** and enabled only by repository policy, treat a socket-enabled repository as trusted to exercise Docker host authority. Verify repository trust before enabling this compatibility mode.

Runner containers should not otherwise receive privileged mode, host networking, host PID/IPC namespaces, or arbitrary host filesystem mounts. The Docker socket, when explicitly enabled, remains the intentional privileged compatibility boundary for Linux runners.

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
4. Keep direct Docker-socket access disabled unless a repository explicitly requires Docker daemon compatibility; for every socket-enabled repository, verify that the repository and everyone who can trigger its workflows are trusted to exercise Docker host authority.
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

The API socket is therefore an authorization boundary, not a sandbox. A Linux runner that has been explicitly granted host Docker socket access retains Docker host authority. Protect the socket filesystem location and keep GitRun's runtime state directory accessible only to the service/dashboard accounts that require it.

## VM-backed runners

VM-backed Logic Containers can route jobs to Docker daemons inside KVM/libvirt or VirtualBox guests. Protect the guest Docker endpoint and base disk image as trusted infrastructure. VM configuration does not remove the need to review repository trust, workflow permissions or guest credentials.

## Hardening controls

GitRun now exposes explicit host-pressure thresholds. With resource-pressure protection enabled, the scheduler leaves existing workloads untouched but stops creating new runners or placing additional queued work while CPU, memory, or state-filesystem usage is above the configured limits.

Runner package caches default to repository isolation. Global cache sharing is an explicit configuration choice.

Windows VM-backed runners require Docker TLS credentials, and dynamic Windows runners are ephemeral. Linux runners can additionally select a Docker network, seccomp profile, and AppArmor profile.

Unexpected runner exits are security events: the container is quarantined and removed rather than restarted in place. GitDockRun resource operations also require an exact repository/run/job/requester binding.

Release updates support signed manifests through Ed25519 and can require signature verification. Release artifacts include dependency metadata, SPDX SBOM, and provenance.
