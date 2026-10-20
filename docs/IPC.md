# GitRun IPC and socket architecture — 1.5.1

GitRun has two distinct communication concepts: the workflow-facing API socket and the internal GSR/executor authentication protocol.

## Workflow-facing socket

Default path:

```text
/run/gitrun/api.sock
```

The path can be overridden with `GITRUN_API_SOCKET`.

The `gitrun-api` launcher connects through a Unix domain socket on Unix hosts. The socket is created with mode `0660` and a dedicated GitRun socket group. This is an OS-level permission boundary, but the host-side peer/container identity check remains authoritative.

The client sends one JSON request terminated by a newline and reads newline-delimited responses.

## Request identity

A request includes:

- GitRun API;
- operation;
- repository;
- workflow;
- optional run ID;
- job;
- runner;
- logical resource;
- validated API arguments.

The launcher populates these from the current GitHub Actions environment and its command-line arguments. Workflow-visible identity values are claims, not credentials.

There is deliberately no workflow-visible bearer token in the wire request.

## Response stream

Responses can acknowledge acceptance and then stream:

- `Started`;
- `Stdout`;
- `Stderr`;
- `Finished { exit_code }`.

This keeps the transport compatible with handlers that produce live command output without changing the public API model.

## GSR -> executor authentication

The internal executor protocol is independent from the socket transport. A `SignedRequest` contains:

- an authorized operation;
- a timestamp;
- a random nonce;
- an HMAC-SHA256 MAC.

The executor verifies the MAC, timestamp and nonce replay state before accepting the request.

The default maximum clock skew is 30 seconds and the replay cache is bounded to 4096 nonces.

This separation means the transport can later move to another protected local channel without changing the authorization contract.

## Identity and replay hardening

Before privileged API execution, the host:

1. maps the socket peer to its current runner container through SO_PEERCRED/cgroup data;
2. rejects stale/PID-reused container identities;
3. requires the associated GitHub job to be currently `in_progress`;
4. resolves the authoritative workflow/run/job from GitHub;
5. checks the runner/container binding for GitDockRun operations.

This prevents a historical job on the same runner, a reused container name, or a caller-selected different job from satisfying authorization.

## Trust boundary

The socket does not make an untrusted process privileged. The host-side authorization layer must establish runner/workflow identity and apply the repository policy before an operation reaches `gitrun-exe`.

The Docker socket remains a separate host-level trust boundary on Linux.


## Verification and deployment boundaries

The PR #83 source HEAD [`5afbe14`](https://github.com/Vider06/GitRun/commit/5afbe14ef3144b8e27770e2058943eff8614bc7c) passed the listed 1.5.0 CI/security/format checks before merge. That is historical commit-scoped evidence, not a blanket guarantee for later changes or an end-to-end adversarial test of every runner disconnect, container replacement and cross-process race.

PR [#103](https://github.com/Vider06/GitRun/pull/103) repairs legacy runtime ownership and canonical systemd unit installation; the workflow socket and GSR/executor protocol remain the separate boundaries described here. Check current commit status and use the [1.5.1 release notes](v1.5.1-release-notes.md) for recovery and deployment requirements.
