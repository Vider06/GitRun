# Git*Run API reference — 1.5.0

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
- Before entering the privileged worker, the backend persists the `attaching` lease and reloads the effective source policy. It refuses live mounts if any visible thread in the bound container cgroup has `CAP_SYS_ADMIN` in CapEff, CapPrm, CapInh, CapAmb or CapBnd, and fails closed if those capability masks cannot be inspected. An independent policy writer is not serialized after the final policy read, so concurrent external policy-file replacement remains a known race.
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


## v1.5.0 verification boundary

PR #83 CI exercises the configured API/runtime-contract and security regression suites. The live GitDockRun mount test exercises cleanup in a privileged Docker namespace, but it is not the same as driving the full authorized disconnect endpoint end-to-end with a real client lifecycle. Treat that complete API path as unverified until separate evidence is recorded. Current-head results and all remaining limitations are tracked in [v1.5.0-refactor-status.md](v1.5.0-refactor-status.md).
