# Security model

This document states plainly what GitRun's design trusts, and what an
operator is implicitly granting when they run it. It documents the security
properties that exist today and the important limits that remain.

## Docker socket access is host-level privilege

Direct Docker-socket access for workflow runners is **disabled by default**. It is a repository-specific compatibility opt-in controlled by `DockerPolicy::direct_socket_enabled`.

For a repository that explicitly enables it, a Linux runner container is started with:

```
--volume /var/run/docker.sock:/var/run/docker.sock
--group-add <docker socket GID>
```

**This is equivalent to granting the runner container root-equivalent access
to the Docker host it runs on.** Anyone who can execute arbitrary code inside
a Linux runner that holds the Docker socket can, at minimum:

- start new containers with arbitrary mounts, including mounting the host's
  root filesystem (`-v /:/host`) and reading/writing anything on it;
- start privileged containers, which can load kernel modules, modify host
  network configuration, and generally escape the container boundary
  entirely;
- read the configuration and environment variables of other containers on
  the host, including other GitRun runners' registration tokens while they're
  briefly live.

This compatibility mode is intentional for workflows that genuinely need
direct Docker daemon access, such as image builds, service containers, or
Docker actions. It is not required for GitDockRun, the Git*Run API, or normal
runner operation.

GitRun also applies Docker-level hardening to Linux runners by default:
all Linux capabilities are dropped and only `CHOWN`, `SETUID`,
`SETGID`, and `DAC_OVERRIDE` are added back, and
`no-new-privileges` is enabled for the hardened socket-compatibility path. **This does not remove the Docker-socket
privilege itself.** A process that can use the mounted socket still has the
Docker API authority described above.

The hardening can only be disabled through the explicit unsafe-runner
configuration gate; it is not a substitute for removing the socket.

Windows container runners are different: the current implementation does
**not** mount the Docker socket or apply the Linux hardening flags, because
those Docker/Unix-kernel mechanisms are not supported there. Windows runner
hardening remains weaker and is an explicit follow-up area.

Practical implications for an operator:

- Keep direct Docker-socket access disabled unless the repository genuinely
  requires it.
- Only enable it for repositories where you trust everyone who can trigger
  a workflow run. If workflows execute untrusted pull-request code on a
  socket-enabled self-hosted runner, that code must be treated as having
  runner/host-level consequences because of the Docker socket.
- Runners should not share a host with anything sensitive unless that
  sensitive material is protected by other means.
- The registration token passed to a Linux runner container
  (`RUNNER_TOKEN`) is a short-lived GitHub runner token rather than GitRun's
  long-lived authentication credential, but it is still exposed through
  container inspection to anyone who already has Docker access on the host.

## Filesystem isolation

Linux runner containers currently keep their root filesystem writable so normal
GitHub Actions jobs can install tools and packages. Filesystem isolation is
instead provided by:

- a tmpfs **or per-runner Docker volume** at
  `/home/runner/actions-runner`, containing runner registration state,
  diagnostics, and job checkouts;
- a 256 MiB tmpfs at `/tmp`;
- a small tmpfs at `/run/gitrun` for GitRun policy/event state;
- a mounted volume for the shared package-manager cache (Cargo/pip/npm).

This limits persistent runner state and keeps temporary/runtime data separate,
but it is **not a read-only-root sandbox**. It does not protect the host
filesystem from a process that can use the Docker socket when direct socket
compatibility has been explicitly enabled.

Windows runners currently do not receive the Linux `--read-only`/tmpfs/PID
hardening, so this isolation model should **not** be assumed to apply to
Windows containers.

## GSR (GitSecureRun)

GSR is partially implemented today.

The standalone `gitrun-gsr` process is a separate watchdog with its own
systemd unit. It watches the scheduler's PID file and records an unexpected
scheduler exit as a critical security event, with graphical notification when
available and terminal/log fallback.

The scheduler also contains an active GSR polling/integration path for
runner command-policy enforcement and Docker-side observation. These controls
are defense-in-depth; they do not turn a Docker-socket runner into a
host-isolated sandbox.

The GSR project is therefore **not merely planned**, but its broader
hardening scope is not complete. In particular, the standalone GSR service
unit itself still needs additional systemd sandboxing review, and the
container escape/socket boundary remains the fundamental trust boundary.

## GitVault and secrets at rest

GitVault is implemented and stores secrets as **AES-256-GCM ciphertext** on
disk. A randomly generated 32-byte master key is stored separately with
owner-only (`0600`) permissions. Secrets are decrypted only in memory when
they are resolved for a runner.

GitVault supports global, group, and repository scopes; more-specific scopes
override less-specific ones for the same secret name.

Encryption at rest protects the stored ciphertext against disk theft,
backup leakage, and casual inspection. It does **not** protect secrets
against a fully compromised host or an attacker who can obtain both the vault
storage and its master key.

GitVault also reports decryption failures through an event-sink interface,
but the default vault instance uses a no-op sink; full external security-event
wiring is still an integration area rather than something operators should
assume is always active.

## Remaining hardening limits

The hardening baseline is now configurable and enforceable, but some controls
remain deployment-dependent:

- Docker networks still require the operator to provide the actual egress
  boundary (firewall, proxy, or isolated network).
- AppArmor custom profiles are opt-in because profile availability is host-specific.
- VM snapshot rollback is an operator-controlled primitive, not an automatic
  per-job snapshot policy.
- GitVault's master/root key remains trusted infrastructure even though new data
  records use a separate derived encryption key.
- Release manifest signing is mandatory in the official release workflow.
  The updater requires a signed manifest by default; setting
  GITRUN_UPDATE_SIGNATURE_REQUIRED=false is an explicit compatibility/unsafe
  opt-out for custom unsigned manifests.

If your threat model requires stronger isolation than this today, consider
running GitRun's Docker host itself inside a dedicated VM rather than
alongside other workloads, until the remaining isolation limitations are
addressed.


## Git*Run API boundary

The workflow-facing Git*Run API is deliberately closed. The available API names and operation verbs are compiled into `gitrun-core`; configuration can disable or narrow them but cannot create a new API/operation pair. Requests are validated before authorization and are converted into `gitrun-exe::AuthorizedOperation` only after repository policy, resource policy and GSR command policy allow them.

The default workflow transport is the Unix socket `/run/gitrun/api.sock`. It carries structured identity and arguments, not a reusable workflow bearer token. The private GSR-to-executor handoff adds HMAC-SHA256 authentication, a random nonce, timestamp checks and replay protection.

This boundary does not reduce Docker-socket privilege. A workflow that can already control the Linux runner's Docker socket can still exercise Docker host authority. The API is intended to constrain GitRun-specific privileged operations, not to turn a privileged runner into a host-isolated sandbox.

## VM boundary

Logic Containers can route jobs into a Docker daemon inside a KVM/libvirt or VirtualBox VM. This can provide a stronger infrastructure boundary than placing the runner directly on the GitRun host, but the guest Docker daemon, base disk image, guest credentials and hypervisor remain trusted infrastructure. VM-backed runners should be used when the deployment threat model benefits from that additional boundary; they do not make an untrusted GitHub repository automatically safe.

## Security hardening baseline

The scheduler treats the Unix API peer, Docker-managed runner identity, and GitHub workflow/run/job records as one authorization chain. Values supplied through GITHUB_* environment variables are claims only; the host verifies the runner container from SO_PEERCRED/cgroups and resolves the authoritative workflow run/job through GitHub before GSR authorization.

The API socket is created with 0660 permissions and a dedicated numeric group. Runner containers join the socket group at startup. Filesystem permissions are defense in depth; the host-side peer/container identity check remains authoritative.

GitDockRun read/write/execute/melt operations require an exact persistent dock binding for repository, workflow run, job, requester runner, and target container. A container identifier alone does not authorize access, and melt cannot cross those trust-domain bindings.

Runner package caches default to per-runner Docker volumes. Repository/global scopes are explicit cross-workflow trust decisions.

Linux runners attach to a dedicated GitRun Docker network, created automatically when absent, and use Docker's configured seccomp/AppArmor policy. The dedicated network is a stable segregation point for deployment firewall/proxy policy; it is not itself an egress firewall.

Windows VM-backed runners require a Docker TLS credential directory and are resolved through Docker TLS. Dynamic Windows runners are ephemeral. The VM is treated as persistent infrastructure, while runner state is disposable.

An unexpectedly exited runner is recorded as a critical security event and quarantined instead of being restarted or reused. Runner removal also asks Docker to remove anonymous volumes and explicitly removes the per-runner home volume when applicable.

GitVault now derives a separate data-encryption key from the root/master key for new records, while retaining legacy decryption for pre-hardening records. Secret read/write/delete operations can be emitted through the existing GSR event bridge without logging secret values.

Release manifests support Ed25519 signatures. The updater can require a signature and can optionally bind verification to a configured key identifier. Releases also publish Cargo dependency metadata, an SPDX 2.3 SBOM, and build provenance.

VM resolution performs a Docker endpoint health check before a VM becomes routable. This check verifies daemon reachability/version/API only; it is not hardware/guest attestation. KVM/libvirt and VirtualBox snapshot create/restore primitives are operator-controlled rollback primitives, not automatic per-job rollback.

The scheduler has a host resource-pressure backpressure gate. By default, when CPU pressure, memory use, or state-filesystem use reaches its threshold, GitRun does not create additional runners or reconcile new placement. Existing workloads are left running and queued GitHub jobs remain queued until pressure drops.

## Trust-boundary summary

Trusted infrastructure: GitRun scheduler, host Docker daemon, GitHub API identity records, hypervisor, VM base image, Docker TLS credentials, and GitVault root key.

Untrusted input: workflow code, workflow-visible environment variables, requested API arguments, logical dock/resource names, repository configuration supplied by workflow code, and downloaded update artifacts until verified.

A successful request therefore requires both authenticated transport identity and authoritative workload binding; policy alone is never treated as proof of identity.
