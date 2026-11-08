#!/usr/bin/env bash
set -euo pipefail

readonly verus="${VERUS_PATH:-${RUNNER_TEMP:-/tmp}/verus/installed/verus}"
readonly results="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-verus-proof-results.tsv"
readonly -a production_sources=(
  "crates/gitrun-core/src/api_policy.rs"
  "crates/gitrun-core/src/runner.rs"
  "crates/gitrun-gsr/src/watchdog.rs"
  "crates/gitrun-recovery/src/json_recovery.rs"
  "crates/gitrun-scheduler/src/backoff.rs"
  "crates/gitrun-smart-scheduler/src/queueing.rs"
  "crates/gitrun-core/src/workflow_validation.rs"
  "crates/gitrun-gsr/src/automation_gate.rs"
  "crates/gitrun-core/src/command_policy.rs"
  "crates/gitrun-core/src/hypervisor_decision.rs"
  "crates/gitrun-core/src/app_auth.rs"
  "crates/gitrun-gsr/src/events.rs"
  "crates/gitrun-gsr/src/agent.rs"
  "crates/gitrun-scheduler/src/docker.rs"
  "crates/gitrun-management/src/control_manager.rs"
  "crates/gitrun-core/src/github_auth.rs"
  "crates/gitrun-core/src/config.rs"
  "crates/gitrun-scheduler/src/reconcile.rs"
  "crates/gitrun-scheduler/src/vm_resolution.rs"
  "crates/gitrun-scheduler/src/logic_containers.rs"
  "crates/gitrun-scheduler/src/dock_registry.rs"
  "crates/gitrun-gsr/src/api_gate.rs"
  "crates/gitrun-exe/src/protocol.rs"
  "crates/gitrun-scheduler/src/resource_pressure.rs"
  "crates/gitrun-core/src/compatibility.rs"
  "crates/gitrun-core/src/settings.rs"
  "crates/gitrun-smart-scheduler/src/future_mapping.rs"
  "crates/gitrun-gsr/src/bin/gitrun-api.rs"
  "crates/gitrun-updater/src/release_policy.rs"
  "crates/gitrun-scheduler/src/vm.rs"
  "crates/gitrun-cli/src/presenter.rs"
  "crates/gitrun-platform/src/lib.rs"
  "crates/gitrun-scheduler/src/github.rs"
  "crates/gitrun-scheduler/src/gtuu.rs"
  "crates/gitrun-gsr/src/exec_supervisor.rs"
  "crates/gitrun-core/src/state.rs"
  "crates/gitrun-management/src/backend.rs"
  "crates/gitrun-recovery/src/lib.rs"
  "crates/gitrun-scheduler/src/lib.rs"
  "crates/gitrun-scheduler/src/state.rs"
  "crates/gitrun-setup/src/lib.rs"
  "crates/gitrun-exe/src/lib.rs"
  "crates/gitrun-smart-scheduler/src/lib.rs"
  "crates/gitrun-updater/src/lib.rs"
  "crates/gitrun-updater/src/state.rs"
  "crates/gitrun-scheduler/src/gsr_poll.rs"
)
phase=preflight

record_result() {
  local source="$1"
  local status="$2"
  printf '%s\t%s\n' "$source" "$status" >> "$results"
}

run_verus_target() {
  local entrypoint="$1"
  shift
  local -a sources=("$@")
  local source
  if "$verus" --cfg verus "$entrypoint"; then
    for source in "${sources[@]}"; do
      record_result "$source" PROVEN
    done
  else
    local rc=$?
    for source in "${sources[@]}"; do
      record_result "$source" FAILED
    done
    return "$rc"
  fi
}

finish_verus_report() {
  local rc=$?
  trap - EXIT
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
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
    output.write("### Verus production proof coverage\n\n")
    output.write("| Result | Sources |\n|---|---:|\n")
    for status in ("PROVEN", "FAILED", "UNVERIFIED"):
        output.write(f"| {status} | {counts[status]} |\n")
    output.write("\n")
    output.write(
        "Only a successful Verus execution is reported as PROVEN. A Verus result "
        "does not substitute for the corresponding Kani result.\n\n"
    )
    output.write(f"Execution phase: {phase} (exit {exit_code}).\n\n")
PY
  fi
  if [ "$rc" -ne 0 ]; then
    echo "Verus formal verification failed during $phase (exit $rc)." >&2
  fi
  exit "$rc"
}
trap finish_verus_report EXIT

mkdir -p "$(dirname "$results")"
: > "$results"

if [[ ! -x "$verus" ]]; then
  echo "Verus executable is missing or not executable: $verus" >&2
  exit 1
fi

for source in "${production_sources[@]}"; do
  if [[ ! -s "$source" ]]; then
    echo "Required Verus production proof source is missing: $source" >&2
    exit 1
  fi
done
if [[ ! -s "verification/kani/updater_policy.rs" ]]; then
  echo "Required updater Verus entrypoint is missing." >&2
  exit 1
fi

echo "Verus executable: $verus"
phase=smoke
echo "Verifying the pinned verifier smoke test."
"$verus" verification/verus/smoke.rs

phase=proof
echo "Verifying the production updater policy through its dedicated proof entrypoint."
run_verus_target "verification/kani/updater_policy.rs" \
  "crates/gitrun-updater/src/policy_logic.rs" \
  "verification/kani/updater_policy.rs"

for source in "${production_sources[@]}"; do
  echo "Verifying production invariants in $source."
  run_verus_target "$source" "$source"
done

phase=complete
