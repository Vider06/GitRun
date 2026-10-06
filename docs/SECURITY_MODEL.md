# Security model

This document states plainly what GitRun's design trusts, and what an
operator is implicitly granting when they run it. It documents the security
properties that exist today and the important limits that remain.

## Docker socket access is host-level privilege

Every GitRun-managed **Linux** runner container is currently started with:

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

This is intentional: GitRun's Linux runners need Docker access because CI
workflows commonly build images, run service containers, or invoke Docker
actions.

GitRun also applies Docker-level hardening to Linux runners by default:
all Linux capabilities are dropped and only `CHOWN`, `SETUID`,
`SETGID`, and `DAC_OVERRIDE` are added back, and
`no-new-privileges` is enabled. **This does not remove the Docker-socket
privilege itself.** A process that can use the mounted socket still has the
Docker API authority described above.

The hardening can only be disabled through the explicit unsafe-runner
configuration gate; it is not a substitute for removing the socket.

Windows container runners are different: the current implementation does
**not** mount the Docker socket or apply the Linux hardening flags, because
those Docker/Unix-kernel mechanisms are not supported there. Windows runner
hardening remains weaker and is an explicit follow-up area.

Practical implications for an operator:

- Only point GitRun at repositories where you trust everyone who can trigger
  a workflow run. If workflows execute untrusted pull-request code on
  self-hosted runners, that code must be treated as having runner/host-level
  consequences because of the Docker socket.
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
filesystem from a process that can use the Docker socket.

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

## What GitRun does *not* currently do

Documented here so these limitations are explicit rather than assumed:

- No dedicated network isolation between runner containers and the rest of
  the host/network beyond Docker's normal networking behavior.
- No custom seccomp/AppArmor profile beyond Docker's defaults.
- No complete host-independent secret protection: GitVault encrypts secrets
  at rest, but the host/master-key trust boundary remains.
- No complete GSR hardening/sandboxing of every component; GSR's crash
  watchdog and scheduler-side controls exist today, while broader hardening
  work remains.

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
