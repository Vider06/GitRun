# `gitrun-exe/src`

Execution-layer implementation. `lib.rs` defines authorized requests and backends; `ipc.rs` defines workflow socket messages; `protocol.rs` provides HMAC, nonce, timestamp and replay protection for the private execution channel.

The private executor channel uses authenticated envelopes, per-request nonces, timestamps and replay protection. Passing unit/integration tests are evidence for the covered cases, not proof that every real runner/container lifecycle race has been exercised; see the [API reference](../../../docs/GITRUN_API.md) and [status tracker](../../../docs/v1.5.0-refactor-status.md).
