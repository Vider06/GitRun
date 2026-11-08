#!/usr/bin/env bash
set -euo pipefail

readonly verus="\${VERUS_PATH:-\${RUNNER_TEMP:-/tmp}/verus/installed/verus}"
readonly -a production_sources=(
  "crates/gitrun-core/src/api_policy.rs"
  "crates/gitrun-core/src/runner.rs"
  "crates/gitrun-gsr/src/watchdog.rs"
  "crates/gitrun-recovery/src/json_recovery.rs"
  "crates/gitrun-scheduler/src/backoff.rs"
  "crates/gitrun-smart-scheduler/src/queueing.rs"
)

if [[ ! -x "$verus" ]]; then
  echo "Verus executable is missing or not executable: $verus" >&2
  exit 1
fi

echo "Verus executable: $verus"
echo "Verifying the pinned verifier smoke test."
"$verus" verification/verus/smoke.rs

echo "Verifying the production updater policy through its dedicated proof entrypoint."
"$verus" --cfg verus verification/kani/updater_policy.rs

for source in "\${production_sources[@]}"; do
  if [[ ! -s "$source" ]]; then
    echo "Required Verus production proof source is missing: $source" >&2
    exit 1
  fi
  echo "Verifying production invariants in $source."
  "$verus" --cfg verus "$source"
done
