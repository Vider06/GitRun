#!/usr/bin/env bash
set -Eeuo pipefail
cd /home/runner/actions-runner
: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,Linux,X64,gitrun,gitrun-ci}"
if [[ ",${RUNNER_LABELS}," != *,gitrun-ci,* ]]; then
  RUNNER_LABELS="${RUNNER_LABELS},gitrun-ci"
fi
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-false}"
RUNNER_DISABLE_UPDATE="${RUNNER_DISABLE_UPDATE:-false}"
SHARED_CACHE_DIR="${GITRUN_SHARED_CACHE_DIR:-/var/lib/gitrun/shared}"

# A Docker named volume mounted at /home/runner/actions-runner replaces the
# image's pre-created directory and defaults to root ownership. Repair that
# mount before dropping privileges to the runner account.
chown -R runner:runner /home/runner/actions-runner

mkdir -p "$SHARED_CACHE_DIR"
chown runner:runner "$SHARED_CACHE_DIR"
sudo -u runner -E mkdir -p   "$SHARED_CACHE_DIR/cargo"   "$SHARED_CACHE_DIR/cargo-target"   "$SHARED_CACHE_DIR/pip"   "$SHARED_CACHE_DIR/npm"

if [[ -f .runner ]]; then
  chown runner:runner .runner .credentials .credentials_rsaparams 2>/dev/null || true
fi

if [[ ! -f .runner ]]; then
  args=(--url "$RUNNER_URL" --token "$RUNNER_TOKEN" --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" --unattended --replace)
  [[ "$RUNNER_EPHEMERAL" == "true" ]] && args+=(--ephemeral)
  [[ "$RUNNER_DISABLE_UPDATE" == "true" ]] && args+=(--disableupdate)
  sudo -u runner -E ./config.sh "${args[@]}"
fi
unset RUNNER_TOKEN
exec sudo -u runner -E ./run.sh
