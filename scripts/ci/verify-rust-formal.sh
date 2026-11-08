#!/usr/bin/env bash
set -euo pipefail

readonly expected_kani_version="0.68.0"
readonly required_harnesses=(
  "update_policy_acceptance_is_exact"
  "release_type_wire_values_are_total"
  "update_policy_wire_values_are_total"
  "api_operation_matrix_matches_the_closed_contract"
  "desired_count_respects_bounds_for_all_u32_inputs"
  "pid_range_is_exact"
  "recovery_action_names_are_total_and_exact"
  "cooldown_seconds_are_bounded"
  "growth_ceiling_is_bounded_and_saturating"
)
readonly policy_harness="verification/kani/updater_policy.rs"
readonly inventory="\${RUNNER_TEMP:-\${TMPDIR:-/tmp}}/gitrun-rust-source-inventory.tsv"
readonly coverage="\${RUNNER_TEMP:-\${TMPDIR:-/tmp}}/gitrun-kani-source-coverage.tsv"
readonly results="\${RUNNER_TEMP:-\${TMPDIR:-/tmp}}/gitrun-kani-proof-results.tsv"
phase=preflight

record_result() {
  local source="$1"
  local status="$2"
  printf '%s\t%s\n' "$source" "$status" >> "$results"
}

run_kani_target() {
  local entrypoint="$1"
  shift
  local -a sources=("$@")
  local source
  if kani "$entrypoint"; then
    for source in "\${sources[@]}"; do
      record_result "$source" PROVEN
    done
  else
    local rc=$?
    for source in "\${sources[@]}"; do
      record_result "$source" FAILED
    done
    return "$rc"
  fi
}

finish_formal_report() {
  local rc=$?
  trap - EXIT

  if [ -s "$inventory" ]; then
    python3 - "$inventory" "$coverage" "$results" <<'PY'
import csv
import sys

inventory, coverage, results = sys.argv[1:]
statuses = {}
try:
    with open(results, newline="", encoding="utf-8") as source:
        for row in csv.reader(source, delimiter="\t"):
            if len(row) == 2:
                statuses[row[0]] = row[1]
except FileNotFoundError:
    pass

with open(inventory, newline="", encoding="utf-8") as source, open(
    coverage, "w", newline="", encoding="utf-8"
) as target:
    reader = csv.reader(source, delimiter="\t")
    writer = csv.writer(target, delimiter="\t", lineterminator="\n")
    writer.writerow(["SOURCE_FILE", "KANI_STATUS", "PACKAGE", "ROLE"])
    for row in reader:
        if len(row) != 4 or row[0] != "INVENTORIED":
            continue
        _, path, package, role = row
        writer.writerow([path, statuses.get(path, "UNVERIFIED"), package, role])
PY
  fi

  if [ -n "\${GITHUB_STEP_SUMMARY:-}" ]; then
    python3 - "$results" "$GITHUB_STEP_SUMMARY" "$phase" "$rc" <<'PY'
import csv
import sys
from collections import Counter
from pathlib import Path

results, summary, phase, exit_code = sys.argv[1:]
counts = Counter()
try:
    with open(results, newline="", encoding="utf-8") as source:
        counts.update(row[1] for row in csv.reader(source, delimiter="\t") if len(row) == 2)
except FileNotFoundError:
    pass
with Path(summary).open("a", encoding="utf-8") as output:
    output.write("### Kani production proof coverage\n\n")
    output.write("| Result | Sources |\n|---|---:|\n")
    for status in ("PROVEN", "FAILED", "UNVERIFIED"):
        output.write(f"| {status} | {counts[status]} |\n")
    output.write("\n")
    output.write(
        "Only completed Kani runs are reported as PROVEN. Sources without an "
        "executed target remain UNVERIFIED; a successful Kani run does not "
        "substitute for the corresponding Verus result.\n\n"
    )
    output.write(f"Execution phase: {phase} (exit {exit_code}).\n\n")
PY
  fi

  if [ "$rc" -ne 0 ]; then
    echo "Kani formal verification failed during $phase (exit $rc)." >&2
  fi
  exit "$rc"
}
trap finish_formal_report EXIT

mkdir -p "$(dirname "$results")"
: > "$results"

if ! command -v kani >/dev/null 2>&1; then
  echo "Kani is not installed; refusing to report formal verification as successful." >&2
  exit 1
fi

kani_version="$(kani --version 2>&1)"
if [[ "$kani_version" != *"$expected_kani_version"* ]]; then
  echo "Unexpected Kani version: $kani_version (expected $expected_kani_version)." >&2
  exit 1
fi

readonly -a proof_sources=(
  "crates/gitrun-core/src/api_policy.rs"
  "crates/gitrun-core/src/runner.rs"
  "crates/gitrun-gsr/src/watchdog.rs"
  "crates/gitrun-recovery/src/json_recovery.rs"
  "crates/gitrun-scheduler/src/backoff.rs"
  "crates/gitrun-smart-scheduler/src/queueing.rs"
)

for source in "\${proof_sources[@]}"; do
  if ! test -s "$source"; then
    echo "Required Kani proof source is missing: $source" >&2
    exit 1
  fi
done

if ! test -s "$policy_harness"; then
  echo "Required updater Kani entrypoint is missing: $policy_harness" >&2
  exit 1
fi

phase=coverage
for harness in "\${required_harnesses[@]}"; do
  found=false
  for source in "\${proof_sources[@]}" "crates/gitrun-updater/src/policy_logic.rs"; do
    if grep -B5 -F "fn $harness()" "$source" | grep -Fq "#[kani::proof]"; then
      found=true
      break
    fi
  done
  if [ "$found" != true ]; then
    echo "Required Kani proof harness or its proof attribute is missing: $harness" >&2
    exit 1
  fi
done

proof_count="$(grep -Rho --include='*.rs' '#\[kani::proof\]' crates | wc -l | tr -d '[:space:]')"
if [[ "$proof_count" -lt "\${#required_harnesses[@]}" ]]; then
  echo "Formal verification has too few proof harnesses: $proof_count." >&2
  exit 1
fi

echo "Kani version: $kani_version"
echo "Required production proof harnesses: \${required_harnesses[*]}"
echo "Total source proof annotations: $proof_count"

phase=proof
run_kani_target "$policy_harness" \
  "crates/gitrun-updater/src/policy_logic.rs" \
  "verification/kani/updater_policy.rs"

for source in "\${proof_sources[@]}"; do
  run_kani_target "$source" "$source"
done

phase=complete
