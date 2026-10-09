# GitRun IPC and socket architecture — 1.4.4

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
