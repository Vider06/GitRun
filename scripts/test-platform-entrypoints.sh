#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

entrypoints=(
  "crates/gitrun-cli/src/main.rs"
  "crates/gitrun-scheduler/src/main.rs"
  "crates/gitrun-gsr/src/main.rs"
  "crates/gitrun-gsr/src/bin/gitrun-api.rs"
  "crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs"
  "crates/gitrun-recovery/src/main.rs"
  "crates/gitrun-dashboard-tauri/src-tauri/src/main.rs"
)

check_guard() {
  local file="$1"
  if ! awk '
    /^fn main\(\)/ {
      in_main = 1
      main_line = NR
      next
    }
    in_main && /ensure_supported_runtime\(\)/ {
      guard_line = NR
      exit
    }
    END {
      if (!main_line || !guard_line || guard_line - main_line > 6) {
        exit 1
      }
    }
  ' "$file"; then
    echo "Android runtime guard is missing or not near the start of fn main(): $file" >&2
    return 1
  fi
  printf 'PASS %s\n' "$file"
}

for entrypoint in "${entrypoints[@]}"; do
  test -f "$entrypoint" || {
    echo "Expected GitRun entry point is missing: $entrypoint" >&2
    exit 1
  }
  check_guard "$entrypoint"
done

echo "All GitRun executable entry points reject unsupported Android runtimes before dispatch."
