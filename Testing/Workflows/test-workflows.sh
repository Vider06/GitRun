#!/usr/bin/env bash
set -euo pipefail
test -d .github/workflows
if command -v actionlint >/dev/null 2>&1; then
  actionlint
else
  docker run --rm -v "$PWD:/repo" --workdir /repo rhysd/actionlint:1.7.12
fi
echo "Workflow checks passed."
