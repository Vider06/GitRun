#!/usr/bin/env bash
set -euo pipefail
manifest="Core/manifests/premade.yaml"
test -f "$manifest"
grep -Fq "schema: gitrun-premade/v1" "$manifest"
grep -Fq "status: active" "$manifest"
grep -Fq "required_metadata: true" "$manifest"
grep -Fq "immutable_references: true" "$manifest"
grep -Fq "signed_release_metadata: true" "$manifest"
grep -Fq "runner-linux-x86_64" "$manifest"
grep -Fq "Dockers/runners/linux-x86_64" "$manifest"
echo "Core manifest checks passed."
