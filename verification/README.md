# GitRun formal verification

GitRun uses Kani for bit-precise model checking of selected pure Rust logic.

## Complementary Rust checks

The CI workflow also applies these complementary checks:

- `cargo fmt --all -- --check` and per-file `rustfmt --check`, using each owning package's declared edition.
- Cargo workspace metadata validation and a fail-closed owner mapping for every tracked `.rs` file.
- Workspace Clippy (`-D warnings`), documentation builds, and Cargo tests at package/target level. The Dashboard, Recovery and Management packages have dedicated build or headless validation paths where they are excluded from broad workspace commands.
- `cargo audit` and `cargo deny` for advisory and dependency-policy analysis.
- Miri runs the targeted pure updater regression test `canonical_premade_runner_source_is_immutable` from `crates/gitrun-updater/src/lib.rs`. It is not presented as whole-workspace Miri coverage.
- The source inventory, Kani coverage and Miri coverage are separate artifacts. Files not covered by a Kani harness or the targeted Miri test remain explicitly `UNVERIFIED` or `NOT IN SCOPE`.

Kani, Miri, Clippy, tests and dependency analysis answer different questions. The source inventory ensures every tracked Rust file is discovered, assigned to a real Cargo package (except the explicit standalone Kani harness), and parsed/formatted individually; Cargo target-level checks then type-check and test the packages and targets that own those files. Rust modules cannot generally be compiled independently of their parent crate because they rely on crate-level imports, macros, features and module visibility.

Verus, Creusot and Flux remain future deductive/refinement-verification candidates: they require deliberate contracts and annotations, not a generic per-file switch, so they are not reported as active gates.

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

## Per-file Rust source assurance

The CI source-assurance gate inventories every tracked `.rs` file, including
test, build-script, example, benchmark, and verification sources. Each file
must be tracked, have an owning Cargo package (or be an explicitly permitted
verification entrypoint), and pass an individual rustfmt parse/format check
with the repository toolchain. The generated inventory records the path,
manifest, and source role; an unowned or malformed file fails the gate.

Package-level type checking, Clippy, documentation, and behavior tests remain
Cargo target/package checks in their dedicated CI jobs. They are not falsely
reported as independent per-file proofs: Rust modules are compiled through
their owning targets, and conditional/platform-specific modules are covered by
the relevant target checks. Kani proves only its declared harness properties,
not every Rust file.

The source inventory is uploaded as a CI artifact. New tracked Rust files are
discovered automatically, so a stale hand-maintained file count cannot hide
them.

## Complementary assurance gates

Kani is not the only Rust check. The normal CI pipeline also requires:

- `cargo fmt --check`, Clippy with `-D warnings`, and Rust documentation builds.
- Workspace and headless-management tests, plus the updater, security, runtime,
  scheduler, platform, service-manager, and release-contract regression suites.
- `cargo audit` and `cargo deny` for advisory and dependency-policy checks.
- Server and updater builds across x86_64/aarch64 and GNU/musl Linux targets.
- Real Server package creation for all four target variants, plus independent
  GNU Desktop DEB/RPM/Arch package creation for x86_64 and ARM64.
- Native musl Desktop builds for x86_64 Alpine and ARM64 Void, followed by the
  x86_64 APK and both-architecture XBPS package checks.
- The one-time 1.9.1 legacy migration DEB, including maintainer-script behavior,
  package identity, and the legacy 1.8.23 packaging contract.
- Release-side package inspection after all package matrices and before signing.

## Formal coverage status vocabulary

- `PROVEN`: the declared Kani harness completed successfully.
- `FAILED`: a declared proof assertion or Kani proof run failed.
- `UNVERIFIED`: the Rust file is covered by source/Cargo gates but has no
  declared Kani property yet.
- `ERROR`: the verifier, toolchain, or proof setup could not run reliably.
- `NOT IN SCOPE`: the item is not a Rust source property represented by the
  current formal model.

The per-file Kani coverage artifact intentionally marks only
`crates/gitrun-updater/src/policy_logic.rs` as `PROVEN`; other Rust files stay
`UNVERIFIED` until meaningful properties are added. This avoids claiming that
three updater-policy harnesses formally prove the entire repository.
Verus remains a planned next-stage verifier; it is not reported as active until
production properties can be proved rather than duplicated in a disconnected
model.
