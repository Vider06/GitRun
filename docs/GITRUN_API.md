# Git*Run API reference — <!-- gitrun-version:start -->1.8.22<!-- gitrun-version:end -->

GitRun exposes a closed set of workflow-facing APIs. These are named executables, not a generic shell wrapper.

## Transport

Default Unix socket:

```text
/run/gitrun/api.sock
```

Override with:

```text
GITRUN_API_SOCKET=/custom/path/api.sock
```

The client sends one newline-delimited JSON `WireRequest`. Responses are newline-delimited `WireResponse` values: `Accepted`, `Error`, or an execution event.

The API launcher obtains identity claims from GitHub Actions variables including `GITHUB_REPOSITORY`, `GITHUB_WORKFLOW`, `GITHUB_RUN_ID`, `GITHUB_JOB` and `RUNNER_NAME`. These are claims only; the host verifies the current runner/container and authoritative GitHub job state before authorization.

## APIs

### GitVaultRun

Operations: `read`, `write`, `exists`, `delete`, `list`.

```text
GitVaultRun --read NAME
GitVaultRun --write NAME VALUE
GitVaultRun --exists NAME
GitVaultRun --delete NAME
GitVaultRun --list
```

Vault resolution follows repository > group > global scope.

### GitDockRun

Operations: `connect`, `disconnect`, `read`, `write`, `execute`, `melt`, `mount` and `unmount`.

```text
GitDockRun --connect --job "$GITHUB_JOB"
GitDockRun --disconnect --job "$GITHUB_JOB"
GitDockRun --read PATH --docked CONTAINER
GitDockRun --write PATH VALUE --docked CONTAINER
GitDockRun --execute COMMAND --docked CONTAINER
GitDockRun --melt [TARGET] --docked SOURCE
GitDockRun --mount HOST_SOURCE --target CONTAINER_PATH --docked runner [--read-only | --read-write]
GitDockRun --unmount MOUNT_ID --docked runner
```

`connect` records a persistent binding over repository, workflow run, job, requester runner and immutable Docker container ID. The special resource alias `runner` resolves to the authenticated runner's own binding; it is not an arbitrary Docker selector. Use `--disconnect --job "$GITHUB_JOB"` during the active job to drop the binding. Disconnect does not kill the runner that is still executing the workflow; normal scheduler lifecycle cleanup owns that container after the job exits.

Read/write/execute/melt/mount/unmount operations must match an authorized binding. The host revalidates the immutable Docker ID before privileged operations. `melt` is a filesystem transfer into a target followed by source-container removal; it is not a live mount.

`GitDockRun --execute` runs as the unprivileged `runner` account, regardless of the container entrypoint's initial root identity. The backend tokenizes the accepted command and invokes it directly with Docker exec; it does not wrap the input in a shell. The grammar is intentionally limited to one executable and plain whitespace-separated arguments. Shell operators, pipelines, separators, quoting, variable/command expansion, redirection, globs and control characters are rejected before execution. This avoids authorizing only the first token of a compound shell command while a shell executes additional unreviewed commands. For complex scripts, commit the script to the workflow/repository and run it as a normal Actions step so GSR supervises each executed program.

### Host mounts (Linux local Docker only)

Mounts are a separate, privileged capability. A mount request must pass the global GitDockRun API policy, the repository's allowed-container list, the target Logic Container's `mountable` capability, and `docker.allowed_mounts`.

- `HOST_SOURCE` must be an existing absolute host path to a regular file or directory. The path must match an enabled `MountPolicy` rule. An explicit matching deny rule wins; recursive rules match descendant paths.
- The rule's `read_only: true` means the source can only be mounted read-only. Requests default to read-only. `--read-write` requires a matching rule whose `read_only` is false.
- `CONTAINER_PATH` must already exist in the bound container, be under `/workspace/`, `/home/runner/actions-runner/_work/`, `/tmp/gitrun-mounts/` or `/var/lib/gitrun/shared/`, contain no symlink path components, and be empty when it is a directory. File sources require file targets; directory sources require directory targets.
- A successful mount returns a generated `mount_id`. Use that ID with `--unmount`; the backend verifies ownership by repository/run/job/requester/container before removing the mount.
- The host keeps a private, cross-process-locked mount registry and reconciles records after workflow jobs complete. Registry updates are atomically renamed and the containing directory is synced. Host sources are staged in an isolated private mount namespace, checked against the pinned source inode, cloned into a detached mount tree and attached by descriptor inside the target namespace. Read-only enforcement uses `mount_setattr`; the detached tree is made read-only before attachment. Unmount rechecks the expected kernel mount ID inside the target namespace before using the validated target path.
- Before entering the privileged worker, the backend persists the `attaching` lease and reloads the effective source policy. It refuses live mounts if any visible thread in the bound container cgroup has `CAP_SYS_ADMIN` in CapEff, CapPrm, CapInh, CapAmb or CapBnd, and fails closed if those capability masks cannot be inspected. Supported GitRun settings saves and mount operations share a sidecar advisory lock. Direct privileged edits that bypass this protocol are not serialized and remain outside the supported writer contract.
- Directory source binds are deliberately non-recursive: nested mounts below the source directory are not copied.
- **Not supported in v1:** Windows, macOS, and remote/VM Docker daemons. The service fails closed outside a local Linux Docker host. A container with direct Docker socket access remains a separate host-level authority boundary.

Mount policy fields alone are not sufficient: the operation must be globally enabled and the destination Logic Container must have `mountable: true`.

### GitSaveRun

Operations: `file`, `logs`.

```text
GitSaveRun --file PATH
GitSaveRun --logs --namefile NAME
```

### GitRegisterRun

Registers a workflow path for the current run:

```text
GitRegisterRun --name NAME --entry PATH [--permanent]
```

### GitInstallRun

Operations: `install`, `remove`, `update`.

```text
GitInstallRun PACKAGE [VERSION]
GitInstallRun --remove PACKAGE
GitInstallRun --update PACKAGE
```

Package identifiers are validated before they reach the container package manager.

### GitReadRun

```text
GitReadRun --read PATH
```

Only approved workflow filesystem prefixes are accepted. Paths are opened component-by-component without following symlinks; reads reject non-regular and hard-linked targets and are capped at 16 MiB.

### GitWriteRun

```text
GitWriteRun --write PATH VALUE
```

The same descriptor-based path checks apply. Writes are capped at 16 MiB and reject symlinked path components and hard-linked targets.

### GitVerifyRun

```text
GitVerifyRun --read PATH
```

The operation computes a SHA-256 digest through a checked file descriptor, rejects symlinked path components and hard-linked/non-regular files, and caps hashing at 1 GiB.

### GitStatusRun

```text
GitStatusRun --status
```

Reports GitRun status through the API path.

## Authorization

The authorization sequence is fail-closed:

1. API/operation pairing must exist in the source-defined contract.
2. Required and allowed arguments are validated.
3. Effective repository API policy must permit the operation.
4. Resource policy must permit the requested resource.
5. The host verifies the peer process/cgroup maps to the current managed runner.
6. The current GitHub job must be `in_progress`; stale or historical jobs are rejected.
7. Workflow/run identity is checked against the authoritative GitHub workflow run.
8. GitDockRun requires an exact persisted dock binding and immutable container identity.
9. GitDockRun execute operations are additionally checked by the GSR command policy.
10. Only then is an `AuthorizedOperation` created.

Policy intersection can only remove capabilities; it cannot grant a capability absent from the broader policy.

## Private executor authentication

The GSR-to-executor protocol uses:

- 32-byte random channel keys;
- HMAC-SHA256;
- a 32-byte random nonce per request;
- timestamp validation with a default 30-second clock-skew limit;
- a bounded replay cache.

The public workflow API does not expose this channel key.

## Security note

The API is an authorization boundary, not a replacement for host isolation. A Linux runner that already has Docker socket access retains host-level Docker authority. See [SECURITY_MODEL.md](SECURITY_MODEL.md).


## Verification and deployment boundaries

The CI result linked from PR [#83](https://github.com/Vider06/GitRun/pull/83) is historical evidence for the 1.5.0 source-head checks only. It does not certify later recovery/version-alignment commits or prove an end-to-end adversarial test of every API operation.

PR [#103](https://github.com/Vider06/GitRun/pull/103) changed recovery diagnostics and repair of the host runtime/systemd installation; it does not change the closed Git*Run API operation set described above. Follow the [release history](RELEASE_HISTORY.md) and check the exact workflow results for the commit being deployed. The complete authorized disconnect API path and real host/hypervisor lifecycle tests remain environment-specific verification obligations.

### GitDispatchRun

`GitDispatchRun --plan --target-job <job-id> --target-workflow <path> --target-labels <csv>` asks the host-side GitRun controller for an optional Kitty Dispatch plan.

The response contains machine-readable `GITRUN_DISPATCH_STATUS`, `GITRUN_DISPATCH_MODE`, `GITRUN_DISPATCH_RUNS_ON` (JSON array) and `GITRUN_DISPATCH_REASON` lines. `MODE=kitty` means the controller validated Kitty settings and a saved workload profile and issued a unique route label; the workflow must use the returned JSON array as `runs-on`. `MODE=github` means the plan has deliberately fallen back to the caller's ordinary labels.

Dispatch requires explicit GSR authorization for `GitDispatchRun/Plan`, Kitty enabled, aggregate telemetry, Adaptive Resource Management, a known workload profile and Shadow mode OFF. Unknown profiles, unsupported VM-backed workloads, invalid settings and other verified non-ready states return an ordinary GitHub fallback instead of fabricating a route. The pre-made planner itself must run on an online dedicated `gitrun-control` runner to reach the local Unix-socket service; if that control runner is unavailable, the planner remains queued and cannot choose a fallback.

GitHub does not expose an API for pinning a queued job directly to a `runner_id`. Kitty Dispatch therefore creates a per-occurrence label that a GitRun-managed ephemeral runner alone carries (in addition to the workload-family label) and relies on GitHub's supported label/group matching. The scheduler verifies the resulting job/container identity. This cannot re-route a job that was already queued with a different `runs-on` selector. A capacity change between planning and runner creation can still cause GitRun to defer creation; operator host limits and GSR checks remain authoritative.
