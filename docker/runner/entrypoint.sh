#!/usr/bin/env bash
set -Eeuo pipefail

cd /home/runner/actions-runner

: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"

RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,Linux,X64,gitrun}"
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-false}"
RUNNER_DISABLE_UPDATE="${RUNNER_DISABLE_UPDATE:-false}"

if [[ ! -f .runner ]]; then
    args=(
        --url "${RUNNER_URL}"
        --token "${RUNNER_TOKEN}"
        --name "${RUNNER_NAME}"
        --labels "${RUNNER_LABELS}"
        --unattended
        --replace
    )

    if [[ "${RUNNER_EPHEMERAL}" == "true" ]]; then
        args+=(--ephemeral)
    fi

    if [[ "${RUNNER_DISABLE_UPDATE}" == "true" ]]; then
        args+=(--disableupdate)
    fi

    ./config.sh "${args[@]}"
fi

exec ./run.sh
