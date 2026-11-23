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
  "crates/gitrun-updater/src/policy_logic.rs"
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
  "crates/gitrun-vault/src/lib.rs"
  "crates/gitrun-setup/src/resources.rs"
  "crates/gitrun-scheduler/src/gsr_poll.rs"
)
phase=preflight

record_result() {
  local source="$1"
  local status="$2"
  printf '%s\t%s\n' "$source" "$status" >> "$results"
}

proof_failures=0

record_verus_diagnostic() {
  local step="$1" command_text="$2" exit_code="$3" output="$4"
  local log="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-ci/logs/verus-${step//[^A-Za-z0-9_.-]/_}.log"
  mkdir -p "$(dirname "$log")"
  printf '%s\n' "$output" > "$log"
  python3 scripts/ci/ci-job-report.py diagnostic \
    --step "$step" --command "$command_text" --exit-code "$exit_code" --log-file "$log"
}

verus_result_counts() {
  local output="$1"
  local result_line
  result_line="$(printf '%s\n' "$output" | grep -E 'verification results:: [0-9]+ verified, [0-9]+ errors' | tail -n 1 || true)"
  local result_pattern='verification results:: ([0-9]+) verified, ([0-9]+) errors'
  if [[ "$result_line" =~ $result_pattern ]]; then
    printf '%s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
    return 0
  fi
  return 1
}

record_successful_verus_results() {
  local output="$1"
  local target="$2"
  local -a sources=("${@:3}")
  local verified_count
  local error_count
  local source
  local counts
  local minimum_expected="${#sources[@]}"
  if (( minimum_expected == 0 )); then
    minimum_expected=1
  fi
  if ! counts="$(verus_result_counts "$output")"; then
    proof_failures=$((proof_failures + 1))
    for source in "${sources[@]}"; do
      record_result "$source" UNVERIFIED
    done
    record_verus_diagnostic "verus-result-summary-$target" "Verus result summary validation for $target" 1 "$output"
    echo "Verus returned success without a parseable verification summary for $target; refusing to mark its sources PROVEN." >&2
    return 1
  fi
  read -r verified_count error_count <<< "$counts"
  if (( verified_count < minimum_expected || error_count != 0 )); then
    proof_failures=$((proof_failures + 1))
    for source in "${sources[@]}"; do
      record_result "$source" UNVERIFIED
    done
    record_verus_diagnostic "verus-result-counts-$target" "Verus verification summary for $target" 1 "$output"
    echo "Verus returned $verified_count verified and $error_count errors for $target (minimum expected: $minimum_expected); refusing to mark its sources PROVEN." >&2
    return 1
  fi
  for source in "${sources[@]}"; do
    record_result "$source" PROVEN
  done
  return 0
}

run_verus_target() {
  local entrypoint="$1"
  shift
  local -a sources=("$@")
  local output
  local rc
  if output="$("$verus" --expand-errors --time --time-expanded --log-all --multiple-errors 100 --cfg verus "$entrypoint" 2>&1)"; then
    printf '%s\n' "$output"
    record_successful_verus_results "$output" "$entrypoint" "${sources[@]}" || true
  else
    rc=$?
    printf '%s\n' "$output"
    record_verus_diagnostic "verus-target-$entrypoint" "$verus --expand-errors --time --time-expanded --log-all --multiple-errors 100 --cfg verus $entrypoint" "$rc" "$output"
    proof_failures=$((proof_failures + 1))
    for source in "${sources[@]}"; do
      record_result "$source" FAILED
    done
    echo "Verus target failed: $entrypoint (exit $rc); continuing to inspect remaining production proof targets." >&2
  fi
  return 0
}

run_verus_package() {
  local package="$1"
  shift
  local -a sources=("$@")
  local output
  local rc
  # Cargo may report an up-to-date target without invoking Verus, which means
  # there is no fresh verifier summary to attribute to this production package.
  # Remove only this package's artifacts so every certification unit executes
  # Verus now; dependencies remain cached.
  cargo clean -p "$package"
  if output="$(cargo -vv verus verify --locked -p "$package" --lib -- --expand-errors --time --time-expanded --log-all --multiple-errors 100 --cfg verus 2>&1)"; then
    printf '%s\n' "$output"
    record_successful_verus_results "$output" "Cargo package $package" "${sources[@]}" || true
  else
    rc=$?
    printf '%s\n' "$output"
    record_verus_diagnostic "verus-package-$package" "cargo -vv verus verify --locked -p $package --lib" "$rc" "$output"
    proof_failures=$((proof_failures + 1))
    for source in "${sources[@]}"; do
      record_result "$source" FAILED
    done
    echo "Verus Cargo package verification failed: $package (exit $rc)." >&2
  fi
  return 0
}

run_verus_binary() {
  local package="$1"
  local binary="$2"
  local source="$3"
  local output
  local rc
  # Force a fresh package build for the binary target as well. An old cached
  # artifact is not evidence that this binary's current sources were verified.
  cargo clean -p "$package"
  if output="$(cargo -vv verus verify --locked -p "$package" --bin "$binary" -- --expand-errors --time --time-expanded --log-all --multiple-errors 100 --cfg verus 2>&1)"; then
    printf '%s\n' "$output"
    record_successful_verus_results "$output" "Cargo binary $package/$binary" "$source" || true
  else
    rc=$?
    printf '%s\n' "$output"
    record_verus_diagnostic "verus-binary-$package-$binary" "cargo -vv verus verify --locked -p $package --bin $binary" "$rc" "$output"
    proof_failures=$((proof_failures + 1))
    record_result "$source" FAILED
    echo "Verus Cargo binary verification failed: $package/$binary (exit $rc)." >&2
  fi
  return 0
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
if ! command -v cargo-verus >/dev/null 2>&1; then
  echo "cargo-verus is missing; multi-crate production proofs require the Cargo integration." >&2
  exit 1
fi

for source in "${production_sources[@]}"; do
  if [[ ! -s "$source" ]]; then
    echo "Required Verus production proof source is missing: $source" >&2
    exit 1
  fi
done

echo "Verus executable: $verus"
mkdir -p "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-ci"
printf 'Verus\t%s\n' "$("$verus" --version 2>&1 | head -n 1)" >> "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-ci/tools.tsv"
printf 'cargo-verus\t%s\n' "$(cargo verus --version 2>&1 | head -n 1 || true)" >> "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-ci/tools.tsv"
phase=smoke
echo "Verifying the pinned verifier smoke test."
smoke_output="$("$verus" --expand-errors --time --time-expanded --log-all --multiple-errors 100 --cfg verus verification/verus/smoke.rs 2>&1)" || {
  smoke_rc=$?
  printf '%s\n' "$smoke_output"
  record_verus_diagnostic "verus-smoke" "$verus verification/verus/smoke.rs" "$smoke_rc" "$smoke_output"
  echo "Verus smoke verification failed (exit $smoke_rc)." >&2
  exit "$smoke_rc"
}
printf '%s\n' "$smoke_output"
if ! record_successful_verus_results "$smoke_output" "Verus smoke test"; then
  exit 1
fi

phase=proof
echo "Verifying the standalone CLI presenter proof."
run_verus_target "crates/gitrun-cli/src/presenter.rs" \
  "crates/gitrun-cli/src/presenter.rs"

run_verus_target "verification/kani/updater_policy.rs" \
  "verification/kani/updater_policy.rs"

declare -a package_order=()
declare -A package_seen=()
for source in "${production_sources[@]}"; do
  if [[ "$source" == "crates/gitrun-cli/src/presenter.rs" || "$source" == "crates/gitrun-gsr/src/bin/gitrun-api.rs" ]]; then
    continue
  fi
  package="${source#crates/}"
  package="${package%%/*}"
  if [[ -z "${package_seen[$package]:-}" ]]; then
    package_seen[$package]=1
    package_order+=("$package")
  fi
done

for package in "${package_order[@]}"; do
  package_sources=()
  for source in "${production_sources[@]}"; do
    if [[ "$source" == "crates/$package/"* && "$source" != "crates/gitrun-gsr/src/bin/gitrun-api.rs" ]]; then
      package_sources+=("$source")
    fi
  done
  echo "Verifying production invariants in Cargo package $package."
  run_verus_package "$package" "${package_sources[@]}"
done

run_verus_binary "gitrun-gsr" "gitrun-api" "crates/gitrun-gsr/src/bin/gitrun-api.rs"

if [ "$proof_failures" -ne 0 ]; then
  echo "Verus found $proof_failures failing production proof target(s); see the per-source diagnostics above." >&2
  exit 1
fi

phase=complete