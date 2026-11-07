#!/usr/bin/env bash
set -euo pipefail

readonly expected_kani_version="0.68.0"
readonly required_harnesses=(
  "update_policy_acceptance_is_exact"
  "release_type_wire_values_are_total"
)

if ! command -v kani >/dev/null 2>&1 || ! command -v cargo-kani >/dev/null 2>&1; then
  echo "Kani is not installed; refusing to report formal verification as successful." >&2
  exit 1
fi

kani_version="$(kani --version 2>&1)"
if [[ "$kani_version" != *"$expected_kani_version"* ]]; then
  echo "Unexpected Kani version: $kani_version (expected $expected_kani_version)." >&2
  exit 1
fi

proof_source="crates/gitrun-updater/src/release_policy.rs"
test -s "$proof_source"
for harness in "${required_harnesses[@]}"; do
  if ! grep -Fq "fn $harness()" "$proof_source" || ! grep -Fq "#[kani::proof]" "$proof_source"; then
    echo "Required Kani proof harness is missing: $harness" >&2
    exit 1
  fi
done

proof_count="$(grep -Rho --include='*.rs' '#\[kani::proof\]' crates | wc -l | tr -d '[:space:]')"
if [[ "$proof_count" -lt "${#required_harnesses[@]}" ]]; then
  echo "Formal verification has too few proof harnesses: $proof_count." >&2
  exit 1
fi

echo "Kani version: $kani_version"
echo "Required proof harnesses: ${required_harnesses[*]}"
echo "Total source proof annotations: $proof_count"
cargo kani -p gitrun-updater
