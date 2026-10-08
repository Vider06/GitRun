# Security model — GitRun 1.3.0

This document states plainly what GitRun's design trusts, and what an operator is implicitly granting when they run it. It documents the security properties that exist today and the important limits that remain.

## Docker socket access is host-level privilege

Direct Docker-socket access for workflow runners is **disabled by default**. It is a repository-specific compatibility opt-in controlled by `DockerPolicy::direct_socket_enabled`.

For a repository that explicitly enables it, a Linux runner container is started with:

```
--volume /var/run/docker.sock:/var/run/docker.sock
--group-add <docker socket GID>
```

**This is equivalent to granting the runner container root-equivalent access to the Docker host it runs on.** Anyone who can execute arbitrary code inside a Linux runner that holds the Docker socket can use Docker host authority.

This compatibility mode is intentional for workflows that genuinely need direct Docker daemon access. It is not required for GitDockRun, the Git*Run API, or normal runner operation.

GitRun also applies Docker-level hardening to Linux runners by default: all Linux capabilities are dropped and only `CHOWN`, `SETUID`, `SETGID`, and `DAC_OVERRIDE` are added back, and `no-new-privileges` is enabled for the hardened socket-compatibility path. **This does not remove Docker-socket privilege itself.**

The hardening can only be disabled through the explicit unsafe-runner configuration gate.

Windows runners are only created against configured VM-backed Docker daemons, never the local Linux daemon. They retain the common CPU/memory/network/cache policy, do not receive Linux-only filesystem/capability flags, and request Hyper-V container isolation by default for an additional Windows kernel boundary. Disabling Hyper-V isolation requires the explicit unsafe-runner gate. The VM remains the primary guest boundary for Windows runners.

## Git*Run API trust boundary

The workflow-facing API is a closed operation set. Policy can disable operations, but workflow code cannot create new API/operation pairs.

Authorization is fail-closed. Workflow-visible `GITHUB_*` values are claims only. The host maps the Unix peer to the current Docker runner, rejects stale/PID-reused cgroup identities, requires an in-progress GitHub job, resolves authoritative workflow/run information, and applies repository/resource policy before creating an `AuthorizedOperation`.

GitDockRun adds an exact persistent binding over repository, workflow run, job, requester runner and immutable Docker container ID. `connect`/`disconnect` cannot select an arbitrary other job. Read/write/execute/melt operations revalidate the binding immediately before privileged execution, preventing container-name reuse/TOCTOU authorization.

A successful request therefore requires authenticated transport identity **and** authoritative workload binding; policy alone is never treated as proof of identity.

## Filesystem isolation

Linux runner containers currently keep their root filesystem writable so normal GitHub Actions jobs can install tools and packages. Filesystem isolation is instead provided by:

- a tmpfs **or per-runner Docker volume** at `/home/runner/actions-runner`;
- a 256 MiB tmpfs at `/tmp`;
- a small tmpfs at `/run/gitrun`;
- a mounted volume for the package-manager cache.

This is a read-only-root sandbox for normal Linux runner containers. It is not a substitute for the Docker-socket trust boundary: a runner explicitly granted the host Docker socket still has Docker host authority.

Windows runners do not receive the Linux read-only-root/tmpfs/PID hardening; their security boundary is the guest VM and Windows container model.

## GSR (GitSecureRun)

GSR provides defense-in-depth through command/resource policy, workflow validation, Docker-side observation, security events and external process supervision.

The standalone `gitrun-gsr` process is a watchdog with its own systemd unit. It watches the scheduler PID file and records unexpected scheduler exits as critical security events. The broader GSR hardening scope is still not a substitute for the Docker socket/host boundary, and the standalone service unit still warrants additional systemd sandboxing review.

## GitVault and secrets at rest

GitVault stores secrets as AES-256-GCM ciphertext on disk. A randomly generated 32-byte master key is stored separately with owner-only (`0600`) permissions.

GitVault supports global, group and repository scopes; more-specific scopes override less-specific ones for the same secret name.

New records derive a separate data-encryption key from the root/master key. This is **cryptographic context separation**, not a distinct key per repository or trust domain. Legacy records remain decryptable through the compatibility path.

Secret read/write/delete activity can be bridged into GSR security events without logging secret values. Direct secret reads intentionally return plaintext to an authorized caller; this is not a global plaintext-ban/redaction guarantee for every external output surface. The default standalone Vault sink is still a no-op unless the production integration supplies the GSR event sink.

## Cache, network and sandbox isolation

Runner package caches default to **per-runner Docker volumes**. Repository/global cache scopes are explicit broader trust decisions and should not be used as a substitute for trust-domain isolation when workflows are mutually untrusted.

Managed runners attach to a dedicated GitRun Docker network. GitRun rejects ordinary `bridge`, `host` and `container:*` network modes. The dedicated network is a stable segregation point for deployment firewall/proxy policy; it does **not** itself enforce egress filtering.

Docker uses the host's configured seccomp/AppArmor policy. GitRun-specific hardened AppArmor/seccomp profiles are not yet universally shipped because their availability and compatibility are host/deployment dependent. Operators requiring stronger syscall/LSM confinement should provide and enforce suitable profiles explicitly.

## VM boundary and lifecycle

VM-backed runners use KVM/libvirt or VirtualBox. Windows Docker endpoints require Docker TLS credentials and mutually authenticated TLS. Private guest-IP validation applies across supported hypervisors.

VM Docker endpoint verification checks reachability, daemon version and API response. It is deliberately called **endpoint health verification**, not hardware/guest attestation.

Snapshot create/restore primitives are available for operator-controlled rollback. `EphemeralSnapshotRollback` restores a configured clean snapshot around the workload lifecycle when the VM can be stopped successfully. `EphemeralVm` exists in the lifecycle model, but full VM recreation remains deployment dependent.

## Resource pressure

GitRun has a host resource-pressure backpressure gate. CPU pressure is based on actual CPU utilization rather than load-average/CPU-count semantics. Memory pressure uses configured memory thresholds.

Disk pressure checks configured critical filesystem paths, with defaults covering GitRun and Docker state. Operators with separate VM/storage/cache filesystems should add those paths explicitly.

When pressure reaches a configured threshold, GitRun stops creating additional runners/reconciling new placement. Existing workloads remain running and queued GitHub jobs remain queued until pressure drops.

## Release integrity

Official releases use signed update manifests. The release workflow uses a repository-scoped GitHub Actions secret containing the Ed25519 signing key and a non-secret signing key identifier. The private key is materialized only in the ephemeral workflow workspace for signing and is removed after the signing step. The corresponding public key is distributed to update hosts and the manifest carries the signature and configured signing key identifier.

The updater verifies untrusted manifests at load/fetch/stage boundaries. Official release verification requires signatures; `GITRUN_UPDATE_SIGNATURE_REQUIRED=false` is an explicit compatibility/unsafe opt-out for custom unsigned manifests.

Release artifacts also publish dependency metadata, an SPDX 2.3 SBOM and build provenance. These are supply-chain evidence, not a substitute for signature verification.

## Remaining hardening limits

- Direct Docker-socket compatibility remains host-level privilege.
- Dedicated Docker networking requires an operator-provided egress firewall/proxy policy.
- GitRun-specific seccomp/AppArmor profiles are not universally shipped.
- Windows relies more heavily on VM/guest isolation than Linux.
- Snapshot rollback is operator-controlled and requires a trusted clean snapshot.
- Full ephemeral VM recreation is not yet a universal built-in lifecycle.
- GitVault's root/master key remains trusted infrastructure.
- The default standalone Vault event sink is not a guarantee of external audit delivery.

If your threat model requires stronger isolation than this today, consider running GitRun's Docker host itself inside a dedicated VM rather than alongside other workloads.

## Trust-boundary summary

Trusted infrastructure: GitRun scheduler, host Docker daemon, GitHub API identity records, hypervisor, VM base image, Docker TLS credentials, GitHub Actions release-signing secret, trusted release public key and GitVault root key.

Untrusted input: workflow code, workflow-visible environment variables, requested API arguments, logical dock/resource names, repository configuration supplied by workflow code, and downloaded update artifacts until verified.

A successful privileged request therefore requires both authenticated transport identity and authoritative workload binding; policy alone is never treated as proof of identity.
