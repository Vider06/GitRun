# GitRun formal verification

GitRun uses Kani for bit-precise model checking of selected pure Rust logic.

## Required gate

- Kani version: `0.68.0`
- Production proof target: `gitrun-updater/src/policy_logic.rs`
- Standalone Kani entrypoint: `verification/kani/updater_policy.rs`
- Required harnesses:
  - `update_policy_acceptance_is_exact`
  - `release_type_wire_values_are_total`
  - `update_policy_wire_values_are_total`

Run locally after installing the pinned Kani release:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
bash scripts/ci/verify-rust-formal.sh
```

The pure policy module is shared directly by the updater and the standalone Kani entrypoint, so proofs analyze production code without compiling the updater's HTTP, cryptography, archive, or operating-system integration dependencies. No Kani dependency is added to production builds.

The gate fails if Kani is missing, the pinned version differs, a required harness disappears, or any proof fails. It does not claim that every GitRun crate or runtime behavior is formally proven; the proof inventory must expand as additional production invariants are selected.

The release workflow runs the same source proofs after its package matrices complete, then inspects every produced release package individually before allowing the verified payload to reach signing.
