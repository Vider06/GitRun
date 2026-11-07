#!/usr/bin/env bash
set -euo pipefail

readonly expected_kani_version="0.68.0"
readonly required_harnesses=(
  "update_policy_acceptance_is_exact"
  "release_type_wire_values_are_total"
  "update_policy_wire_values_are_total"
)
readonly proof_source="crates/gitrun-updater/src/policy_logic.rs"
readonly inventory="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-rust-source-inventory.tsv"
readonly coverage="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-kani-source-coverage.tsv"
phase=preflight

finish_formal_report() {
  local rc=$? status
  trap - EXIT
  case "$phase" in
    complete) status=PROVEN ;;
    proof) status=FAILED ;;
    coverage) status=UNVERIFIED ;;
    *) status=ERROR ;;
  esac

  if [ -s "$inventory" ]; then
    python3 - "$inventory" "$coverage" "$status" "$proof_source" <<'PY'
import csv
import sys

inventory, coverage, overall, proof_source = sys.argv[1:]
with open(inventory, newline="", encoding="utf-8") as source, open(
    coverage, "w", newline="", encoding="utf-8"
) as target:
    reader = csv.reader(source, delimiter=chr(9))
    writer = csv.writer(target, delimiter=chr(9), lineterminator="\n")
    writer.writerow(["SOURCE_FILE", "KANI_STATUS", "PACKAGE", "ROLE"])
    for row in reader:
        if len(row) != 4 or row[0] != "FORMAT_OK":
            continue
        _, path, package, role = row
        if overall == "PROVEN":
            status = "PROVEN" if path == proof_source else "UNVERIFIED"
        elif overall == "FAILED":
            status = "FAILED" if path == proof_source else "UNVERIFIED"
        elif overall == "UNVERIFIED":
            status = "UNVERIFIED"
        else:
            status = "ERROR"
        writer.writerow([path, status, package, role])
PY
  fi

  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo "### Kani proof coverage"
      echo
      echo "| Scope | Status |"
      echo "|---|---|"
      echo "| Updater release-policy proof harnesses | **$status** |"
      echo "| Production source file with Kani properties | $proof_source |"
      echo "| Other tracked Rust files | UNVERIFIED by Kani; checked by the per-file and Cargo gates |"
      echo
      echo "Kani status vocabulary: PROVEN, FAILED, UNVERIFIED, ERROR, NOT IN SCOPE."
      echo
    } >> "$GITHUB_STEP_SUMMARY"
  fi
  if [ "$rc" -ne 0 ]; then
    echo "Kani formal verification status: $status (exit $rc)." >&2
  fi
  exit "$rc"
}
trap finish_formal_report EXIT

if ! command -v kani >/dev/null 2>&1; then
  echo "Kani is not installed; refusing to report formal verification as successful." >&2
  exit 1
fi

kani_version="$(kani --version 2>&1)"
if [[ "$kani_version" != *"$expected_kani_version"* ]]; then
  echo "Unexpected Kani version: $kani_version (expected $expected_kani_version)." >&2
  exit 1
fi

if ! test -s "$proof_source"; then
  echo "Required Kani production proof source is missing: $proof_source" >&2
  exit 1
fi
phase=coverage
for harness in "${required_harnesses[@]}"; do
  if ! grep -B1 -F "fn $harness()" "$proof_source" | grep -Fq "#[kani::proof]"; then
    echo "Required Kani proof harness or its proof attribute is missing: $harness" >&2
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
phase=proof
kani verification/kani/updater_policy.rs
phase=complete
