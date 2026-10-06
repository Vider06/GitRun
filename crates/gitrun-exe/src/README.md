# `gitrun-exe/src`

Execution-layer implementation. `lib.rs` defines authorized requests and backends; `ipc.rs` defines workflow socket messages; `protocol.rs` provides HMAC, nonce, timestamp and replay protection for the private execution channel.