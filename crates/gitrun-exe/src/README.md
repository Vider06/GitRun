# `gitrun-exe/src`

Execution-layer implementation. `lib.rs` defines authorized requests and backends; `ipc.rs` defines workflow socket messages; `protocol.rs` provides HMAC, nonce, timestamp and replay protection for the private execution channel.

The protocol tests cover authenticated envelopes, payload tampering, nonces, timestamps and replay handling. The current `ExecutionAuthority` signing/verification round-trip is in-process; it is not a separate-process security boundary. See the [API reference](../../../docs/GITRUN_API.md) and [status report](../../../docs/v1.5.0-refactor-status.md).
