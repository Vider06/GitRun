#!/usr/bin/env bash
set -euo pipefail
image="gitrun-premade/runner-linux-x86_64:test"
docker buildx version
docker buildx build --check --platform linux/amd64 Dockers/runners/linux-x86_64
docker buildx build --load --platform linux/amd64 --tag "$image" Dockers/runners/linux-x86_64
docker image inspect "$image" >/dev/null
docker image inspect "$image" --format '{{.Config.User}}' | grep -Fxq "gitrun"
echo "Docker runner image checks passed."
