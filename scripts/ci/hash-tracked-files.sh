#!/usr/bin/env bash
# Deterministic cache fingerprint for tracked files.
#
# Avoid GitHub Actions hashFiles() expression evaluation on large workspaces:
# some hosted runner jobs can spend minutes in that pre-step expansion. This
# script hashes only the existing tracked files matching the supplied git
# pathspecs, includes each path in the digest, and emits an empty string when
# no files match (the same cache-key fallback behavior expected by callers).
set -euo pipefail

if (($# == 0)); then
  echo "usage: hash-tracked-files.sh <git pathspec>..." >&2
  exit 2
fi

mapfile -d '' -t listed_paths < <(git ls-files -z -- "$@" | LC_ALL=C sort -z)
paths=()
for path in "${listed_paths[@]}"; do
  [[ -f "$path" ]] && paths+=("$path")
done

if (("${#paths[@]}" == 0)); then
  exit 0
fi

{
  for path in "${paths[@]}"; do
    printf '%s\0' "$path"
    sha256sum -- "$path" | cut -d ' ' -f 1
    printf '\0'
  done
} | sha256sum | cut -d ' ' -f 1
