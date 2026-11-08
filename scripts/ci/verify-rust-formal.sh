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
  "dock_operation_wire_names_are_total"
  "runner_floor_predicate_matches_the_capacity_contract"
  "automation_action_wire_names_are_total"
  "repository_component_byte_alphabet_is_closed"
  "violation_action_config_mapping_is_total"
  "simple_command_byte_alphabet_is_closed"
  "decision_filename_byte_code_is_injective"
  "decision_filename_literal_alphabet_is_closed"
  "github_app_numeric_id_must_be_positive"
  "github_app_id_string_validation_rejects_invalid_values"
  "severity_critical_classification_is_exact"
  "shell_dispatch_never_crosses_shell_families"
  "workload_routing_byte_alphabet_is_exact"
  "workload_routing_id_requires_canonical_prefix_length_and_hex_case"
  "control_manager_action_allowlist_is_closed"
  "matching_quote_bytes_is_exact_over_all_byte_pairs"
  "env_token_unquoting_preserves_unmatched_and_unquoted_values"
  "ascii_digit_classifier_is_exact"
  "repository_component_byte_alphabet_is_exact"
  "config_id_and_repository_boundaries_are_exact"
  "bounded_runner_target_respects_limits_for_all_u32_inputs"
  "only_rfc1918_ipv4_octets_are_accepted"
  "ascii_lowercase_label_byte_is_exact_over_all_u8_values"
  "workload_label_comparison_is_ascii_case_insensitive"
  "dock_binding_identity_requires_every_field_and_positive_run_id"
  "api_operation_authorization_is_fail_closed"
  "channel_key_nonce_and_mac_lengths_are_exact"
  "timestamp_skew_check_is_symmetric_and_overflow_safe"
  "memory_measurements_reject_impossible_available_values"
  "host_memory_reserve_never_underflows"
  "compatibility_severity_aggregation_is_total_and_ordered"
  "repository_override_cannot_reenable_global_api"
  "predictive_peak_is_bounded_for_all_u32_inputs"
  "api_parser_rejects_duplicate_arguments_and_operations"
  "release_type_marker_is_a_closed_three_value_contract"
)
readonly policy_harness="verification/kani/updater_policy.rs"
readonly inventory="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-rust-source-inventory.tsv"
readonly coverage="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-kani-source-coverage.tsv"
readonly results="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-kani-proof-results.tsv"
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
)

for source in "${proof_sources[@]}"; do
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
for harness in "${required_harnesses[@]}"; do
  found=false
  for source in "${proof_sources[@]}" "crates/gitrun-updater/src/policy_logic.rs"; do
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
if [[ "$proof_count" -lt "${#required_harnesses[@]}" ]]; then
  echo "Formal verification has too few proof harnesses: $proof_count." >&2
  exit 1
fi

echo "Kani version: $kani_version"
echo "Required production proof harnesses: ${required_harnesses[*]}"
echo "Total source proof annotations: $proof_count"

phase=proof
run_kani_target "$policy_harness" \
  "crates/gitrun-updater/src/policy_logic.rs" \
  "verification/kani/updater_policy.rs"

for source in "${proof_sources[@]}"; do
  run_kani_target "$source" "$source"
done

phase=complete
