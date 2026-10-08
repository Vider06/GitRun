#!/usr/bin/env bash
set -euo pipefail

readonly premade="Core/Dockers/runners/linux-x86_64/Dockerfile"
readonly source="docker/runner/Dockerfile"

git fetch --quiet origin main --depth=1

test -f "$premade"
test -f "$source" || git show "FETCH_HEAD:$source" > /tmp/gitrun-setup-dockerfile

git show "FETCH_HEAD:$source" > /tmp/gitrun-setup-dockerfile
cmp -s /tmp/gitrun-setup-dockerfile "$premade"

grep -Fq "GITRUN_GSR_COMMAND_POLICY_ENABLED=true" "$premade"
grep -Fq 'ENTRYPOINT ["/entrypoint.sh"]' "$premade"

echo "Premade setup Dockerfile is synchronized with GitRun."
