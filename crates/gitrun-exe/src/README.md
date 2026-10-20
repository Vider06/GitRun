# `gitrun-exe/src`

Execution-layer implementation. `lib.rs` defines authorized requests and backends; `ipc.rs` defines workflow socket messages; `protocol.rs` provides HMAC, nonce, timestamp and replay protection for the private execution channel.

The protocol tests cover authenticated envelopes, payload tampering, nonces, timestamps and replay handling. The current `ExecutionAuthority` signing/verification round-trip is in-process; it is not a separate-process security boundary. See the [API reference](../../../docs/GITRUN_API.md), [1.5.1 release notes](../../../docs/v1.5.1-release-notes.md) and [historical 1.5.0 status report](../../../docs/v1.5.0-refactor-status.md). The 1.5.1 change set concerns guarded host recovery/version metadata; it does not convert this in-process protocol round-trip into a separate-process trust boundary.
