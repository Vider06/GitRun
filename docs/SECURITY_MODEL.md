# Security model — GitRun 1.5.0

## Optional Kitty Smart Scheduler

Kitty is a separate, optional statistics-based add-on. A missing Kitty settings
file means disabled. Invalid configuration, unsafe/corrupt model state, stale
observations, insufficient history, or persistence failure must not block the
ordinary scheduler and must never increase its configured runner floor. The
deterministic reconciler remains the default and the fallback.

Kitty stores only bounded, aggregated per-repository observations: queued-job
counts, counts of busy/online/managed runners, and optional host-wide CPU,
memory, and disk percentages. It does not collect secrets, tokens, environment
values, workflow contents, command strings, or per-job payloads. The historical
model and decision audit are size-bounded and written via an owner-checked
private directory and atomic replacement.

The only automated action implemented in this version is an opt-in increase of
the minimum runner floor. It requires the master switch, the separate predictive
warm-pool switch, shadow mode disabled, fresh data, sufficient history, forecast
quality above the operator threshold, and room under the configured global
maximum. The increase is also capped by an explicit additional-runner bound.
Existing resource-pressure backpressure suppresses predictive growth. Kitty
cannot lower the configured minimum, change security policy, authorize an API,
execute commands, call Docker, read Vault secrets, force-cancel workflows, or
terminate processes. Adaptive resource management and Smart Fast Stop triggers
remain reserved and inactive until they have reliable workload-level evidence
and their own verified safety contracts.


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

GitDockRun adds an exact persistent binding over repository, workflow run, job, requester runner and immutable Docker container ID. `connect`/`disconnect` cannot select an arbitrary other job. Disconnect drops the active job's binding without deleting its live runner; normal scheduler lifecycle owns cleanup after the job exits. Read/write/execute/melt/mount/unmount operations revalidate the binding immediately before privileged execution, preventing stale container-name bindings from authorizing a replacement container.

A successful request therefore requires authenticated transport identity **and** authoritative workload binding; policy alone is never treated as proof of identity.

## GitDockRun live host mounts

GitDockRun's `mount` and `unmount` operations are a separate, privileged capability and are implemented only for a local Linux Docker daemon. A mount is accepted only if the global GitDockRun API policy, repository container allowlist, target Logic Container's `mountable` capability and repository host-source `MountPolicy` all allow it. The host source is canonicalized and opened before the namespace operation; explicit matching deny rules win. The default CLI mode is read-only, and a policy marked `read_only: true` cannot be used for a writable mount.

The runtime pins the target using the immutable Docker container ID, checks the host PID against the container cgroup, validates a restricted existing target path, and performs bind/remount/unmount inside that container's mount namespace. Host sources are staged in an isolated private mount namespace, checked against the pinned source inode, cloned into a detached mount tree and attached by descriptor. The containing target mount is isolated from peer propagation without recursively changing the whole container mount tree. Read-only attributes are applied to the detached tree before attachment; unmount rechecks the expected kernel mount ID inside the target namespace. Before live operations, the helper inspects CapEff, CapPrm, CapInh, CapAmb and CapBnd for every visible thread in the bound container cgroup and fails closed if inspection is incomplete or CAP_SYS_ADMIN is present. Mount leases are stored in a private, cross-process-locked registry before the privileged side effect, then updated with the observed kernel mount ID; registry updates are atomically renamed and the directory is synced. Unmount requires the lease ID to match repository, run, job, requester and container identity. A previous full CI run passed the live read-only directory and read-write file bind/unmount smoke test; later adversarial follow-ups remain unverified as recorded in the refactor tracker.

This is not a blanket filesystem sandbox: host paths are highly sensitive, write mounts permit host-file modification, and a runner with direct Docker socket access can exercise separate host-level authority. The helper requires host privileges and must be validated on the deployment's Linux kernel/runtime. Windows, macOS, and remote/VM Docker daemons are not supported by this live-mount implementation and must fail closed. Mount target paths must exist before the request; directory targets must be empty to avoid hiding existing content. If reconciliation preserves a lease, treat it as a recovery journal: inspect the lease ID, immutable container ID, recorded target, source device/inode and kernel mount ID against the live container's mount table before taking action. A `preparing` lease with no kernel ID proves the attach worker was not entered; an `attaching` or later lease without a uniquely verified mount is ambiguous. Do not delete such a registry entry or unmount by pathname alone. Preserve the record and escalate for host-level recovery if the exact original mount cannot be proven; remove the lease only after the exact mount is confirmed absent or safely unmounted.

## Filesystem isolation

Filesystem operations exposed by GitReadRun, GitWriteRun, GitVerifyRun, GitSaveRun and GitDockRun Read/Write execute inside the container as the unprivileged `runner` identity. Their path checks are enforced twice: a lexical allowlist rejects traversal and limits paths to the checkout/work area, temporary storage and explicitly shared data, while the in-container file helper walks directory components through directory file descriptors using `O_NOFOLLOW`. It rejects symbolic links, non-regular targets and files with multiple hard links rather than following them; reads and writes are capped at 16 MiB, hashing at 1 GiB, and GitSaveRun copies are size-limited during the copy and published with an atomic rename. These controls narrow pathname races but are not a substitute for container isolation or OS permissions. `GitInstallRun` is a separate explicitly policy-gated package-install capability that may execute as container root; it does not imply host root unless the Docker socket is separately exposed.

GitDockRun's dock-only lifecycle marker is stored in a dedicated per-container control volume mounted **read-only** into the runner container. The host scheduler changes it through the Docker volume's validated host mountpoint only after confirming the exact volume is mounted read-only on the bound container. The entrypoint fails closed on a missing/writable control mount, symlink, hard-linked marker, unexpected ownership/mode or invalid marker payload. This distinction matters because container-root APIs (including an administrator-enabled package-install operation) can bypass mode 0700 on any volume mounted writable inside the container. Existing runner containers without the new control-volume mount must be recreated before they can transition into dock-only mode.

Linux runner containers mount their root filesystem read-only by default. Writable state is explicitly provided through:

- a per-runner Docker volume at `/home/runner/actions-runner`;
- a 256 MiB tmpfs at `/tmp`;
- a small tmpfs at `/run/gitrun`;
- a mounted package-manager cache volume;
- the optional Docker socket and GitRun API socket mounts when their policies allow them.

This is a read-only-root sandbox for normal Linux runner containers. It deliberately means that workflows needing to mutate the base operating-system filesystem should use a prepared runner image from the Premade ecosystem rather than relying on `apt install`-style mutation. Disabling the read-only root requires the explicit unsafe-runner gate.

Windows runners are always routed to configured VM-backed Docker daemons; GitRun never silently falls back to the local Linux daemon for a Windows workload. They receive the common CPU, memory, network and cache policy and request Hyper-V container isolation by default. Linux-only read-only-root/tmpfs/PID/capability primitives are not silently emulated. Disabling Hyper-V container isolation requires the explicit unsafe-runner gate. The VM remains the primary guest boundary for Windows runners.

The Docker socket remains a separate host-authority boundary: a runner explicitly granted it can control the Docker daemon and therefore is not equivalent to an ordinary sandboxed runner.

## GSR (GitSecureRun)

GSR provides defense-in-depth through command/resource policy, workflow validation, Docker-side observation, security events and external process supervision.

The standalone `gitrun-gsr` process is a watchdog with its own systemd unit. It watches the scheduler PID file and records unexpected scheduler exits as critical security events. The broader GSR hardening scope is still not a substitute for the Docker socket/host boundary, and the standalone service unit still warrants additional systemd sandboxing review.

The shared security-event JSONL queue rotates the active file before it would exceed 16 MiB and retains up to ten archives. On Unix, producers and readers coordinate through a sidecar advisory lock, including during rotation, so current GitRun processes do not append into a renamed file or read across a partial rotation. Readers include retained archives; the newest-event view remains bounded. This is a finite retention policy: oldest archives are deleted when the cap is exceeded, so the queue is not a permanent audit archive. Preserve/export events externally if a longer compliance retention period is required. A single unusually large event can exceed the nominal per-file cap.

## GitVault and secrets at rest

GitVault stores secrets as AES-256-GCM ciphertext on disk. A randomly generated 32-byte master key is stored separately with owner-only (`0600`) permissions.

GitVault supports global, group and repository scopes; more-specific scopes override less-specific ones for the same secret name.

New records derive a separate data-encryption key from the root/master key. This is **cryptographic context separation**, not a distinct key per repository or trust domain. Legacy records remain decryptable through the compatibility path.

Secret read/write/delete activity can be bridged into GSR security events without logging secret values. Direct secret reads intentionally return plaintext to an authorized caller; this is not a global plaintext-ban/redaction guarantee for every external output surface. The default standalone Vault sink is still a no-op unless the production integration supplies the GSR event sink.

## Cache, network and sandbox isolation

Runner package caches default to **per-runner Docker volumes**. Repository/global cache scopes are explicit broader trust decisions and should not be used as a substitute for trust-domain isolation when workflows are mutually untrusted.

Managed Linux runners attach to the stable, GitRun-owned bridge configured by `GITRUN_RUNNER_NETWORK`. That bridge must be created with `com.docker.network.bridge.enable_icc=false`; a network whose ICC option is missing or true is rejected before GitRun starts another Linux runner. This closes direct runner-to-runner port/DNS connectivity on the shared bridge while retaining outbound NAT and a fixed bridge identity for host firewall/proxy policy. It does **not** enforce egress filtering, and operators still need an explicit egress policy.

**Existing-install migration:** older bridges may have ICC enabled. Drain the workflows, stop/remove or disconnect every runner attached to the network, then remove the now-empty managed network (default name: `gitrun-runner`). The scheduler creates it with ICC disabled on the next Linux runner creation. Do not remove a network with connected containers. GitRun deliberately fails closed rather than using an existing bridge whose isolation it cannot verify.

Windows runner network isolation is a separate deployment boundary: Windows containers use a configured VM-backed Docker daemon and do not use the Linux bridge ICC option. Treat the guest VM firewall and per-VM network policy as required controls until live Windows container-to-container isolation is independently verified.

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

Official releases use signed update manifests. The release workflow uses a repository-scoped GitHub Actions secret containing the Ed25519 signing key and a non-secret signing key identifier. The private key is materialized only in the ephemeral workflow workspace for signing and is removed after the signing step. The official GitRun CLI includes the matching trusted public verification key and key identifier (`gitrun-release-ed25519-v1`) as defaults, so existing installations can verify official releases without manually adding a public key to their environment file. Explicit operator values from the process environment or persistent configuration take precedence over these defaults.

The updater verifies untrusted manifests at load/fetch/stage boundaries. Signature verification is required by default for official releases. Setting `GITRUN_UPDATE_SIGNATURE_REQUIRED=false` is an explicit compatibility/unsafe opt-out for custom unsigned manifests and should not be used for official releases.

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


## v1.5.0 Phase 7 verification note — 2026-10-19

PR #83 adds regression coverage for updater signature verification and rollback, privileged dashboard executable trust resolution, VM configuration/resolution, and GitDockRun mount cleanup in a privileged Docker namespace. The current CI source baseline passed GitRun CI, CodeQL, DevSkim, Rustfmt and Cargo Lock Maintenance. Consult the [refactor status tracker](v1.5.0-refactor-status.md) for exact commit/run evidence; documentation-only changes made during Phase 7 need a final-head verification run.

The following are **not** claimed as verified by those automated checks:

- A live desktop `pkexec` authorization dialog and real install/uninstall/reinstall privilege transitions.
- Full KVM/libvirt and VirtualBox provisioning/recovery on actual hypervisor hosts.
- Disconnect cleanup initiated through the complete authorized API endpoint (the live namespace test exercises the cleanup helper).
- Upgrade/migration behavior for every pre-existing or operator-customized runner image and control volume.
- Serialization of direct privileged policy edits that intentionally bypass GitRun's advisory-lock protocol.
- Zero open alerts in GitHub's Security tab; successful CodeQL workflow execution proves the scan ran, not the state of all alerts.

These are deployment/evidence limits, not assurances that the behaviors are safe in every environment. Before production release, validate the relevant environment-specific paths or explicitly accept each residual risk with a named scope and rationale.
