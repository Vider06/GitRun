# GitRun formal verification

GitRun combines ordinary Rust source assurance with narrowly scoped formal
proofs. These gates are deliberately reported separately: formatting,
compilation, tests, static analysis, and package inspection do not become
formal proofs simply because they cover the same source tree.

## Active Kani proofs

Kani is pinned to `0.68.0`. The initial production proof target,
`crates/gitrun-updater/src/policy_logic.rs`, is a dependency-light module
shared by the updater and the standalone harness
`verification/kani/updater_policy.rs`. The current Kani targets now also
include the closed API-operation matrix, runner capacity bounds, scheduler
cooldown ceiling, watchdog PID range, and Kitty queue-growth ceiling.

The current Kani proof targets are:

- `update_policy_acceptance_is_exact`: every combination of the finite
  `UpdatePolicy` and `ReleaseType` domains has the expected acceptance result.
- `release_type_wire_values_are_total` and
  `update_policy_wire_values_are_total`: all policy and release variants have
  their expected stable wire representations.
- `api_operation_matrix_matches_the_closed_contract` in
  `crates/gitrun-core/src/api_policy.rs`: all 200 API/operation combinations
  are checked against the explicit closed contract matrix.
- `desired_count_respects_bounds_for_all_u32_inputs` in
  `crates/gitrun-core/src/runner.rs`: the production runner target is always
  bounded by the configured maximum and by the effective minimum, including
  saturating arithmetic and reversed bounds.
- `cooldown_seconds_are_bounded` in
  `crates/gitrun-scheduler/src/backoff.rs`: the production Retry-After clamp
  never increases its input and never exceeds 86400 seconds.
- `pid_range_is_exact` in `crates/gitrun-gsr/src/watchdog.rs`: the PID guard
  accepts exactly the positive values representable by signed `pid_t`.
- `growth_ceiling_is_bounded_and_saturating` in
  `crates/gitrun-smart-scheduler/src/queueing.rs`: predictive runner growth
  is bounded by both saturating addition and the configured maximum.
- `recovery_action_names_are_total_and_exact` in
  `crates/gitrun-recovery/src/json_recovery.rs`: each privileged recovery
  action has exactly one supported command spelling.
- `dock_operation_wire_names_are_total` in
  `crates/gitrun-core/src/workflow_validation.rs`: every Dock operation keeps
  its canonical external spelling.
- `runner_floor_predicate_matches_the_capacity_contract`,
  `automation_action_wire_names_are_total`, and
  `repository_component_byte_alphabet_is_closed` in
  `crates/gitrun-gsr/src/automation_gate.rs`: the production warm-pool floor
  stays inside its configured/global bounds, automation action names remain
  stable, and repository components accept only the declared ASCII alphabet.
- `violation_action_config_mapping_is_total` and
  `simple_command_byte_alphabet_is_closed` in
  `crates/gitrun-core/src/command_policy.rs`: unknown violation actions fail
  safe to Kill and the direct-argv parser rejects bytes outside its allowlist.
- `decision_filename_byte_code_is_injective` and
  `decision_filename_literal_alphabet_is_closed` in
  `crates/gitrun-core/src/hypervisor_decision.rs`: the VM decision filename
  byte encoding is injective and keeps escaped bytes outside the literal range.
- `github_app_numeric_id_must_be_positive` and
  `github_app_id_string_validation_rejects_invalid_values` in
  `crates/gitrun-core/src/app_auth.rs`: App IDs must be positive decimal
  integers, with explicit zero, non-decimal, and overflow boundary cases.
- The standalone `verification/kani/updater_policy.rs` harness also proves
  representative allow/deny outcomes using the imported production policy.

These targeted properties do not prove the updater's HTTP, cryptography,
archive, filesystem, process, or operating-system integration, nor the
uncovered parts of the GitRun workspace. They are not substitutes for the
remaining per-source obligations in the manifest.

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
from source coverage. The Verus runner invokes a standalone verification target for each production
scope marked `IMPLEMENTED` in `verification/source-proof-map.tsv`. The current
set includes the updater policy, API policy, GSR enforcement and event gates,
command parsing, configuration/authentication, scheduler capacity and routing,
VM identity, management authorization, platform mappings, setup policy, Kitty
workload IDs, runner artifact digests, updater settings and Vault identifiers:

- `UpdatePolicy::accepts_release_type` in
  `crates/gitrun-updater/src/policy_logic.rs`, with all nine policy/release
  combinations checked against the production body.
- `GitRunApi::supports_operation` in
  `crates/gitrun-core/src/api_policy.rs`, with all 200 API/operation pairs
  checked as permitted or denied.
- `Runner::desired_count` in `crates/gitrun-core/src/runner.rs`, with
  minimum/maximum bounds proved for every `u32` input.
- `cap_cooldown_seconds` in `crates/gitrun-scheduler/src/backoff.rs`, with
  the 86400-second cap and exact behavior on either side of the boundary.
- `pid_is_representable` in `crates/gitrun-gsr/src/watchdog.rs`, with the
  exact interval accepted before a value can be converted to `pid_t`.
- `bounded_growth_upper` in
  `crates/gitrun-smart-scheduler/src/queueing.rs`, with overflow-safe growth
  and the operator-configured runner ceiling.
- `JsonRecoveryAction::as_str` in
  `crates/gitrun-recovery/src/json_recovery.rs`, with exact command names
  for all three recovery actions.
- `DockOperation::as_str` in
  `crates/gitrun-core/src/workflow_validation.rs`, with exact operation wire
  names for all six variants.
- The automation gate in `crates/gitrun-gsr/src/automation_gate.rs`, proving
  the runner floor predicate, repository component byte alphabet, and
  automation action wire names.
- The command policy in `crates/gitrun-core/src/command_policy.rs`, proving
  its canonical violation-action names, fail-safe unknown-value default, and
  direct-command byte allowlist.
- The hypervisor decision protocol in
  `crates/gitrun-core/src/hypervisor_decision.rs`, proving the injective
  byte-code mapping and separation of literal and escaped values.
- The GitHub App identity policy in `crates/gitrun-core/src/app_auth.rs`,
  proving the positive-ID predicate for all u64 values.
- The GSR event severity enum in `crates/gitrun-gsr/src/events.rs`, proving
  every severity variant's canonical wire name.

The standalone updater-policy entrypoint also checks representative policy
outcomes through its imported production module.

These are authored proof targets, not a claim that the pinned verifier has
already discharged them. Their actual result is recorded only when the dual
gate executes Kani and Verus.

## Required dual-verifier source gate

`verification/source-proof-map.tsv` is the authoritative manifest for
per-file Kani and Verus coverage. It requires one row for every tracked Rust
source file, with a Kani entrypoint and a Verus entrypoint. The gate discovers
sources from `git ls-files` on every run and fails closed for missing,
duplicate, stale, or structurally disconnected mappings. Each entrypoint
must be either the mapped source itself or a repository-local `#[path]`
module that imports that source, and it must contain the corresponding
verifier proof construct.

The source-closure checks are structural preflight only. They do not establish
that a proof exercises the mapped implementation or that it states meaningful
properties. Every mapping therefore also requires review of the actual proof
and its connection to the production implementation. A successful gate means
both verifier processes completed successfully for every mapped source; a
smoke proof, compilation, inventory entry, or lint result cannot substitute.

**Coverage at this PR revision:** the repository has 75 tracked Rust source
files. Fifty rows are marked `IMPLEMENTED` and have production-linked Kani
and Verus proof entrypoints registered in the two verifier runners. Twenty-five
rows are `NOT_APPLICABLE`: these are process/UI/transport adapters, build
scripts, integration-test fixtures, and the verifier smoke fixture whose
behavior is covered by their owning modules or host integration checks rather
than an independent pure production invariant. No row remains `PLANNED`.
The implemented properties cover policy and authorization, bounded arithmetic,
GSR event and enforcement behavior, authenticated executor protocol limits,
repository/workload/VM identity grammars, Docker and service-manager mapping,
memory and runner capacity bounds, setup resource transformation, release
digest and policy validation, and Vault identifier validation.

These counts describe authored and registered proof targets, not `PROVEN`
results. A source is reported as fully covered only after both verifier
processes complete successfully for that source and the dual-verifier audit
reconciles their per-source result files. Kani/Verus compilation failures,
missing results, or a smoke proof cannot substitute for source-level proof.
Every `NOT_APPLICABLE` row has separate Kani and Verus rationale in the TSV
and is not counted as a formal proof. The gate remains closed until every
applicable scope has successful Kani and Verus results and every non-applicable
scope has a reviewed, accepted boundary; no source is silently excluded.

The TSV has separate `KANI_OBLIGATIONS`, `KANI_CASES`, and
`VERUS_OBLIGATIONS` columns so the required property and the input partitions
are explicit for every source.

Run the strict map gate locally only after preparing both verifier entrypoints
for every tracked source:

```sh
python3 scripts/ci/verify-rust-dual-coverage.py
```

The report records per-tool outcomes and a combined scope state:
`PROVEN`, `FAILED`, `UNVERIFIED`, `NOT_APPLICABLE`, `PLANNED`, or
`UNMAPPED`. A source is fully proven only when both its Kani and Verus
statuses are `PROVEN`. A justified `NOT_APPLICABLE` source is never counted
as proven; it records that the source is a build/test/bootstrap harness rather
than production logic and names the CI check that covers it. Structural
readiness is not published as a proof status.

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
