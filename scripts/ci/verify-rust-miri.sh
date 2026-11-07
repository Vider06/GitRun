#!/usr/bin/env bash
set -euo pipefail

readonly toolchain="nightly-2026-11-06"
readonly test_name="canonical_premade_runner_source_is_immutable"
readonly source_file="crates/gitrun-updater/src/lib.rs"
readonly coverage="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-miri-source-coverage.tsv"
readonly summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

cargo "+$toolchain" miri test --locked -p gitrun-updater --lib -- --list \
  | grep -F "$test_name: test" >/dev/null

cargo "+$toolchain" miri test --locked -p gitrun-updater --lib "$test_name"

printf 'MIRI_TESTED\t%s\t%s\n' "$source_file" "$test_name" > "$coverage"
{
  echo "### Miri interpreter coverage"
  echo
  echo "| Scope | Status |"
  echo "|---|---|"
  echo "| \`$source_file::$test_name\` | **MIRI_TESTED** |"
  echo "| Other Rust sources | NOT IN SCOPE for this targeted Miri run |"
  echo
  echo "Miri executes one deterministic, pure updater-source test under the interpreter. It does not claim whole-workspace Miri coverage or replace Kani, Clippy, Cargo tests, or package checks."
  echo
} >> "$summary"
