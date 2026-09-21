#!/usr/bin/env bash
set -Eeuo pipefail
cd /home/runner/actions-runner
: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,Linux,X64,gitrun}"
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-false}"
RUNNER_DISABLE_UPDATE="${RUNNER_DISABLE_UPDATE:-false}"
SHARED_CACHE_DIR="${GITRUN_SHARED_CACHE_DIR:-/var/lib/gitrun/shared}"

mkdir -p "$SHARED_CACHE_DIR"
chown runner:runner "$SHARED_CACHE_DIR"
sudo -u runner -E mkdir -p   "$SHARED_CACHE_DIR/cargo"   "$SHARED_CACHE_DIR/cargo-target"   "$SHARED_CACHE_DIR/pip"   "$SHARED_CACHE_DIR/npm"

if [[ ! -f .runner ]]; then
  args=(--url "$RUNNER_URL" --token "$RUNNER_TOKEN" --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" --unattended --replace)
  [[ "$RUNNER_EPHEMERAL" == "true" ]] && args+=(--ephemeral)
  [[ "$RUNNER_DISABLE_UPDATE" == "true" ]] && args+=(--disableupdate)
  sudo -u runner -E ./config.sh "${args[@]}"
fi
unset RUNNER_TOKEN
exec sudo -u runner -E ./run.sh
