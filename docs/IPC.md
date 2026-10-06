# GitRun IPC and socket architecture

GitRun has two distinct communication concepts: the workflow-facing API socket and the internal GSR/executor authentication protocol.

## Workflow-facing socket

Default path:

```text
/run/gitrun/api.sock
```

The path can be overridden with `GITRUN_API_SOCKET`.

The `gitrun-api` launcher connects through a Unix domain socket on Unix hosts. It sends one JSON request terminated by a newline and reads newline-delimited responses.

The socket is a transport boundary only. The request still carries workflow identity and is subject to GSR authorization.

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

The launcher populates these from the current GitHub Actions environment and its command-line arguments.

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

## Trust boundary

The socket does not make an untrusted process privileged. The host-side authorization layer must establish runner/workflow identity and apply the repository policy before an operation reaches `gitrun-exe`.

The Docker socket remains a separate host-level trust boundary on Linux.
