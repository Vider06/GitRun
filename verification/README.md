# GitRun formal verification

GitRun combines ordinary Rust source assurance with narrowly scoped formal
proofs. These gates are deliberately reported separately: formatting,
compilation, tests, static analysis, and package inspection do not become
formal proofs simply because they cover the same source tree.

## Active Kani proofs

Kani is pinned to `0.68.0`. The current production proof target is
`crates/gitrun-updater/src/policy_logic.rs`, which is a dependency-light
module shared by the updater and the standalone harness
`verification/kani/updater_policy.rs`.

The current Kani harnesses prove these properties over the actual shared
implementation:

- `update_policy_acceptance_is_exact`: every combination of the finite
  `UpdatePolicy` and `ReleaseType` domains has the expected acceptance result.
- `release_type_wire_values_are_total`: every release type has its expected
  stable wire representation.
- `update_policy_wire_values_are_total`: every update policy has its expected
  stable wire representation.

These properties do not prove the updater's HTTP, cryptography, archive,
filesystem, process, or operating-system integration, and they do not prove
the rest of the GitRun workspace.

Run the current Kani suite locally after installing the pinned release:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
bash scripts/ci/verify-rust-formal.sh
```

Kani remains outside production dependencies and shipped binaries.

## Verus status

Verus is pinned to `0.2026.11.02.8bf68ee` and installed by
`scripts/ci/install-verus.sh`. Its required Rust toolchain (`1.99.1`, with
`rustc-dev` and `llvm-tools`) is installed separately from GitRun's primary
Rust toolchain (`1.99.0`).

The toolchain smoke proof in `verification/verus/smoke.rs` remains separate
from source coverage. A first production Verus property is now attached to
`crates/gitrun-updater/src/policy_logic.rs`: the production
`UpdatePolicy::accepts_release_type` body is exposed through Verus's
`dual_spec` mechanism, and the proof checks all nine policy/release
combinations. The standalone updater-policy entrypoint also checks that the
production module is reachable from its verifier harness.

These are authored proof targets, not a claim that the pinned verifier has
already discharged them. Their actual result is recorded only when the dual
gate executes Kani and Verus.

## Required dual-verifier source gate

`verification/source-proof-map.tsv` is the authoritative manifest for
per-file Kani and Verus coverage. It requires one row for every tracked Rust
source file, with a Kani entrypoint and a Verus entrypoint. The gate discovers
sources from `git ls-files` on every run and fails closed for missing,
duplicate, stale, reused, or structurally disconnected mappings. Each
entrypoint must reach its mapped source through repository-local
`#[path]` modules and contain the corresponding verifier proof construct.

The source-closure checks are structural preflight only. They do not establish
that a proof exercises the mapped implementation or that it states meaningful
properties. Every mapping therefore also requires review of the actual proof
and its connection to the production implementation. A successful gate means
both verifier processes completed successfully for every mapped source; a
smoke proof, compilation, inventory entry, or lint result cannot substitute.

**Coverage at this PR revision:** the repository has 75 tracked Rust source
files. Two rows now have distinct, source-local Kani and Verus entrypoints:
the updater policy implementation and its standalone verifier harness. The
remaining 73 rows are still `PLANNED`. This is implementation readiness,
not a `PROVEN` result: both verifier processes must still complete
successfully before either row is reported as covered. The gate remains
intentionally red until every tracked Rust file has a reviewed, meaningful
Kani proof and a reviewed, meaningful Verus proof. This includes tests, build
scripts, and verification harnesses; no file is silently excluded.

Run the strict map gate locally only after preparing both verifier entrypoints
for every tracked source:

```sh
python3 scripts/ci/verify-rust-dual-coverage.py
```

The report records only executed outcomes: `PROVEN`, `FAILED`,
`UNVERIFIED`, or `ERROR`. A source is fully covered only when both its Kani
and Verus statuses are `PROVEN`. Structural readiness is not published as a
proof status.

## Per-file Rust source assurance

The separate source-assurance gate inventories every tracked `.rs` file,
including tests, build scripts, examples, benchmarks, and verification
sources. Each file must be assigned to a real Cargo package or to an explicitly
permitted standalone verification entrypoint. Workspace formatting is checked
by the existing `rust-lint` job; standalone verification entrypoints are
formatted individually. The generated inventory records each path, manifest,
and source role, and unowned files fail the gate.

Cargo metadata validation, package-level type checking, Clippy, documentation,
and behavior tests remain in their existing CI jobs. Rust modules are normally
compiled through their owning targets rather than as independent crates.
These checks provide source assurance but are not reported as formal proofs.

## Complementary checks

- `cargo fmt --all -- --check`, workspace Clippy with `-D warnings`, and
  Rust documentation builds run in the existing `rust-lint` job.
- Dedicated test jobs cover workspace and headless-management tests, plus
  updater, security, runtime, scheduler, platform, service-manager, and
  release-contract regression suites.
- `cargo audit` and `cargo deny` run in the existing `audit-rust` job.
- Miri runs the targeted updater regression test
  `canonical_premade_runner_source_is_immutable`. This is not whole-workspace
  Miri coverage.
- Server and updater builds run across x86_64/aarch64 and GNU/musl Linux
  targets. Desktop and package checks remain in their dedicated matrices.

## CI and Release integration

The pull-request CI workflow and the standalone Release workflow both install
the pinned Kani and Verus versions, run the current Kani production proofs,
smoke-test the Verus toolchain, and execute the strict dual-verifier source
gate. The source inventory and Kani, Verus, and Miri coverage reports are
uploaded separately so their different scopes remain visible.

Release package certification runs after the package matrices have produced
their artifacts. It downloads and inspects those artifacts without rebuilding
them, and the signing job depends on the formal-verification and package
certification job. Signing therefore cannot proceed when that gate fails.
The formal source gate remains strict in both workflows; it is not bypassed
for Release.
