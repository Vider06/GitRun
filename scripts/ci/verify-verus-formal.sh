#!/usr/bin/env bash
set -euo pipefail

readonly verus="${VERUS_PATH:-${RUNNER_TEMP:-/tmp}/verus/installed/verus}"

if [[ ! -x "$verus" ]]; then
  echo "Verus executable is missing or not executable: $verus" >&2
  exit 1
fi

echo "Verus executable: $verus"
echo "Verifying the pinned verifier smoke test."
"$verus" verification/verus/smoke.rs

echo "Verifying the production updater policy through its dedicated proof entrypoint."
"$verus" --cfg verus verification/kani/updater_policy.rs
