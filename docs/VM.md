# VM-backed runners — 1.3.0

GitRun can route dynamic runner containers to a Docker daemon running inside a configured VM. This is primarily intended for Windows runner workloads and for Linux workloads that need a VM boundary.

## Hypervisors

### KVM/libvirt

KVM is preferred when available. GitRun uses the host `virsh` and `qemu-img` command-line tools and checks for `/dev/kvm`.

### VirtualBox

VirtualBox is the fallback when KVM is unavailable and VirtualBox is installed. GitRun uses `VBoxManage`.

GitRun does not silently install a hypervisor. The scheduler exposes human-readable installation guidance instead.

## VM definition

Definitions are stored as a JSON array at:

```text
{GITRUN_STATE_DIR}/vm-configs.json
```

Each VM has:

- `name` — unique logical name used by Logic Containers rules.
- `hypervisor` — `Kvm` or `VirtualBox`.
- `base_disk_image` — operator-supplied image to clone.
- `memory_mb` and `cpus` — guest resources.
- `docker_port` — guest Docker daemon port; commonly 2376.
- `activation` — standard lifecycle, always-on experimental mode, snapshot rollback, or ephemeral-VM lifecycle selection.
- `is_windows` — selects Windows-specific runner behavior.

The dashboard writes the same file that the scheduler reads. Missing configuration means no VM targets.

## Logic Containers routing

A Logic Containers rule contains:

- a name;
- one or more GitHub job labels that must all match;
- a backend;
- a runner image.

Backends are:

- `LocalLinux`;
- `Vm { vm_name }`.

Rules are evaluated in order and the first matching rule wins. An empty label list is invalid and never acts as a catch-all.

Example concept:

```text
labels: self-hosted, windows
backend: VM win-host
image: gitrun-runner:windows
```

The VM name is used instead of a raw IP so VM IP changes do not invalidate the routing rule.

## Lifecycle

For a VM-backed runner:

1. The scheduler sees a job whose labels match a VM rule.
2. The VM resolution registry checks whether that VM is already healthy.
3. If necessary, a background worker provisions/starts the VM.
4. GitRun waits for a usable private guest IPv4 address.
5. The scheduler constructs a remote Docker endpoint.
6. Runner containers are created inside that Docker daemon.
7. Containers remain the autoscaling unit; the VM is the backend host.

The Docker endpoint is health-checked for reachability, API response and daemon version before the VM is admitted as routable. This is an endpoint health check, not hardware or guest attestation.

## Snapshot rollback

KVM/libvirt and VirtualBox expose operator-controlled snapshot create/restore primitives. `EphemeralSnapshotRollback` restores the configured clean snapshot before workload admission and restores it again after the workload lifecycle when shutdown succeeds.

The clean snapshot is operator-managed; GitRun does not silently create a trusted baseline snapshot. Snapshot rollback is therefore a lifecycle primitive, not an automatic guarantee of VM immutability.

The `EphemeralVm` mode is represented in the lifecycle model, but full VM recreation remains deployment/operator dependent.

## Base image requirements

GitRun does not provide guest OS images. The operator must supply a compatible base disk image and ensure the guest already has a Docker daemon configured to listen on the configured port.

GitRun also does not automate Windows installation or silently accept hypervisor licensing terms.

## Networking and TLS

For KVM, the Docker endpoint is constructed from the guest IP and configured Docker port. Windows VM-backed endpoints require a Docker TLS credential directory and mutually authenticated Docker TLS.

For VirtualBox, the current model uses a host-side forwarded Docker endpoint, typically `tcp://127.0.0.1:<port>`.

Use a protected Docker endpoint. Do not expose a guest Docker daemon to an untrusted network. Private-IP validation is applied consistently across supported hypervisors rather than only KVM.

## VM security

A VM can provide a stronger boundary than a host Docker runner, but it is not automatically secure merely because it is a VM. Protect the VM's Docker endpoint, guest credentials and base image, and treat the guest as part of the GitRun trusted infrastructure.
