# Git*Run API reference — 1.3.0

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

Operations: `connect`, `disconnect`, `read`, `write`, `execute`, `melt`.

```text
GitDockRun --connect --job JOB
GitDockRun --disconnect --job JOB
GitDockRun --read PATH --docked CONTAINER
GitDockRun --write PATH VALUE --docked CONTAINER
GitDockRun --execute COMMAND --docked CONTAINER
GitDockRun --melt [TARGET] --docked SOURCE
```

`connect` and `disconnect` are restricted to the caller's current workflow job; a caller cannot select another job. A successful connection records a persistent binding containing repository, workflow run, job, requester runner and immutable Docker container identity.

Read/write/execute/melt operations must match that binding. The scheduler revalidates the current Docker container ID immediately before privileged mutation to prevent container-name reuse/TOCTOU attacks. `melt` also requires the source and target to remain inside the caller's exact trust binding.

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

Only approved workflow filesystem prefixes are accepted.

### GitWriteRun

```text
GitWriteRun --write PATH VALUE
```

The same filesystem allowlist and path safety rules apply.

### GitVerifyRun

```text
GitVerifyRun --read PATH
```

The operation computes a SHA-256 digest of an allowed path.

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
