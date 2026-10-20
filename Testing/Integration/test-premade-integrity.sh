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
test -f Core/Dockers/runners/linux-x86_64/Dockerfile
test -f Core/manifests/premade.yaml
test -f README.md
test -f Workflows/GitRun-Dependent/kitty-dispatch-plan.yml
test -f Workflows/GitRun-Dependent/kitty-dispatch-example.yml
grep -Fq 'GitDispatchRun --plan' Workflows/GitRun-Dependent/kitty-dispatch-plan.yml
grep -Fq 'kitty_online' Workflows/GitRun-Dependent/kitty-dispatch-plan.yml
grep -Fq 'gitrun-controller-offline' Workflows/GitRun-Dependent/kitty-dispatch-plan.yml
grep -Fq 'fromJSON(needs.plan.outputs.runs_on)' Workflows/GitRun-Dependent/kitty-dispatch-example.yml
grep -Fq 'GitDispatchRun' Workflows/GitRun-Dependent/README.md
echo "Premade integrity checks passed."
