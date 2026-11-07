# GitRun formal verification

GitRun uses Kani for bit-precise model checking of selected pure Rust logic.

## Required gate

- Kani version: `0.68.0`
- Required crate: `gitrun-updater`
- Required harnesses:
  - `update_policy_acceptance_is_exact`
  - `release_type_wire_values_are_total`

Run locally after installing the pinned Kani release:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
bash scripts/ci/verify-rust-formal.sh
```

The gate fails if Kani is missing, the pinned version differs, a required harness disappears, or any proof fails. It does not claim that every GitRun crate or runtime behavior is formally proven; the proof inventory must expand as additional production invariants are selected.

The release workflow runs the same source proofs after its package matrices complete, then inspects every produced release package individually before allowing the verified payload to reach signing.
