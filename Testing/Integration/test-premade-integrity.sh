#!/usr/bin/env bash
set -euo pipefail
for forbidden in Cargo.toml Cargo.lock crates src target package.json package-lock.json pnpm-lock.yaml yarn.lock; do
  if test -e "$forbidden"; then
    echo "Forbidden GitRun application path found: $forbidden" >&2
    exit 1
  fi
done
for required in Core Dockers VM-Images Workflows Testing Other; do
  test -d "$required"
  test -f "$required/README.md"
done
test -f Dockers/runners/linux-x86_64/Dockerfile
test -f Core/manifests/premade.yaml
test -f README.md
echo "Premade integrity checks passed."
