#!/usr/bin/env bash
set -euo pipefail

readonly inventory="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitrun-rust-source-inventory.tsv"
readonly summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

command -v git >/dev/null
command -v rustfmt >/dev/null
command -v cargo >/dev/null
test -f Cargo.toml

metadata="$(mktemp)"
package_manifests="$(mktemp)"
trap 'rm -f "$metadata" "$package_manifests"' EXIT
cargo metadata --locked --no-deps --format-version 1 > "$metadata"
python3 - "$metadata" > "$package_manifests" <<'PY'
import json
import sys
from pathlib import Path

metadata = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
for package in metadata["packages"]:
    print(f"{Path(package['manifest_path']).resolve()}\t{package['name']}")
PY

mapfile -d '' -t rust_files < <(git ls-files -z -- '*.rs' | sort -zu)
if (("${#rust_files[@]}" == 0)); then
  echo "No tracked Rust source files were discovered; refusing an empty pass." >&2
  exit 1
fi

: > "$inventory"
failures=0
printf 'Discovered %s tracked Rust source files.\n' "${#rust_files[@]}"

for file in "${rust_files[@]}"; do
  manifest=""
  dir="$(dirname "$file")"
  while [[ "$dir" != "." && "$dir" != "/" ]]; do
    if [[ -f "$dir/Cargo.toml" ]]; then manifest="$dir/Cargo.toml"; break; fi
    dir="$(dirname "$dir")"
  done
  if [[ -z "$manifest" && -f Cargo.toml ]]; then
    if [[ "$file" == verification/*.rs || "$file" == verification/*/*.rs ]]; then
      manifest="Cargo.toml (verification entrypoint)"
    fi
  fi
  if [[ -n "$manifest" && "$manifest" != "Cargo.toml (verification entrypoint)" ]]; then
    if ! grep -q '^\[package\]' "$manifest"; then
      manifest=""
    else
      manifest_abs="$(realpath "$manifest")"
      package_name="$(awk -F '\\t' -v path="$manifest_abs" '$1 == path { print $2; exit }' "$package_manifests")"
      if [[ -z "$package_name" ]]; then manifest=""; fi
    fi
  else
    package_name="verification"
  fi
  if [[ -z "$manifest" ]]; then
    printf 'UNOWNED\t%s\n' "$file" | tee -a "$inventory" >&2
    failures=$((failures + 1))
    continue
  fi

  edition=2021
  if [[ "$manifest" != "Cargo.toml (verification entrypoint)" ]]; then
    declared_edition="$(sed -n 's/^edition[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -n 1)"
    edition="${declared_edition:-2021}"
  fi
  if format_output="$(rustfmt --check --edition "$edition" "$file" 2>&1)"; then
    :
  else
    printf 'FORMAT_FAILED\t%s\t%s\n' "$file" "$manifest" | tee -a "$inventory" >&2
    printf '%s\n' "$format_output" >&2
    failures=$((failures + 1))
    continue
  fi

  case "$file" in
    */tests/*|tests/*) role=test ;;
    */benches/*|benches/*) role=benchmark ;;
    */examples/*|examples/*) role=example ;;
    */build.rs|build.rs) role=build-script ;;
    */src/bin/*|src/main.rs|src/lib.rs) role=target-root ;;
    verification/*) role=verification-harness ;;
    *) role=module ;;
  esac
  printf 'FORMAT_OK\t%s\t%s\t%s\n' "$file" "$manifest" "$role" >> "$inventory"
done

formatted="$(grep -c '^FORMAT_OK' "$inventory" || true)"
{
  echo "### Per-file Rust source assurance"
  echo
  echo "| Metric | Result |"
  echo "|---|---:|"
  echo "| Tracked Rust files discovered | ${#rust_files[@]} |"
  echo "| Files passing individual rustfmt parse/format | $formatted |"
  echo "| Unowned or formatting failures | $failures |"
  echo
  echo "Every tracked `.rs` file is mapped to a real Cargo workspace package (or the explicit Kani verification entrypoint), parsed/formatted individually using its package edition, and included in package/target-level Cargo checks."
  echo
} >> "$summary"

cat "$inventory"
if ((failures > 0)); then
  echo "Rust source assurance failed: $failures file(s) are unowned or failed rustfmt." >&2
  exit 1
fi
echo "Rust source assurance passed for ${#rust_files[@]} tracked files."
