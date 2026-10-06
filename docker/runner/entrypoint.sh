#!/usr/bin/env bash
set -Eeuo pipefail

cd /home/runner/actions-runner

: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"

RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,Linux,X64,gitrun,gitrun-ci}"
if [[ ",${RUNNER_LABELS}," != *,gitrun-ci,* ]]; then
  RUNNER_LABELS="${RUNNER_LABELS},gitrun-ci"
fi
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-false}"
RUNNER_DISABLE_UPDATE="${RUNNER_DISABLE_UPDATE:-false}"
SHARED_CACHE_DIR="${GITRUN_SHARED_CACHE_DIR:-/var/lib/gitrun/shared}"

GITRUN_GSR_COMMAND_POLICY_ENABLED="${GITRUN_GSR_COMMAND_POLICY_ENABLED:-true}"
GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED="${GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED:-true}"
GITRUN_GSR_COMMAND_BLACKLIST_ENABLED="${GITRUN_GSR_COMMAND_BLACKLIST_ENABLED:-false}"
GITRUN_GSR_COMMAND_BLACKLIST="${GITRUN_GSR_COMMAND_BLACKLIST:-}"
GITRUN_GSR_COMMAND_WHITELIST_ENABLED="${GITRUN_GSR_COMMAND_WHITELIST_ENABLED:-false}"
GITRUN_GSR_COMMAND_WHITELIST="${GITRUN_GSR_COMMAND_WHITELIST:-}"
GITRUN_GSR_VIOLATION_ACTION="${GITRUN_GSR_VIOLATION_ACTION:-kill}"

GSR_POLICY_DIR=/run/gitrun
GSR_POLICY_FILE=${GSR_POLICY_DIR}/gsr-policy.env
install -d -o root -g root -m 0755 "$GSR_POLICY_DIR"

validate_policy_value() {
  local name="$1" value="$2"
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* ]]; then
    echo "gitrun-runner: refusing GSR policy value with a newline: $name" >&2
    exit 1
  fi
}

validate_policy_value GITRUN_GSR_COMMAND_POLICY_ENABLED "$GITRUN_GSR_COMMAND_POLICY_ENABLED"
validate_policy_value GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED "$GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED"
validate_policy_value GITRUN_GSR_COMMAND_BLACKLIST_ENABLED "$GITRUN_GSR_COMMAND_BLACKLIST_ENABLED"
validate_policy_value GITRUN_GSR_COMMAND_BLACKLIST "$GITRUN_GSR_COMMAND_BLACKLIST"
validate_policy_value GITRUN_GSR_COMMAND_WHITELIST_ENABLED "$GITRUN_GSR_COMMAND_WHITELIST_ENABLED"
validate_policy_value GITRUN_GSR_COMMAND_WHITELIST "$GITRUN_GSR_COMMAND_WHITELIST"
validate_policy_value GITRUN_GSR_VIOLATION_ACTION "$GITRUN_GSR_VIOLATION_ACTION"

umask 077
tmp_policy="$(mktemp "${GSR_POLICY_DIR}/gsr-policy.env.XXXXXX")"
trap 'rm -f "$tmp_policy"' EXIT
{
  printf 'GITRUN_GSR_COMMAND_POLICY_ENABLED=%s\n' "$GITRUN_GSR_COMMAND_POLICY_ENABLED"
  printf 'GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED=%s\n' "$GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED"
  printf 'GITRUN_GSR_COMMAND_BLACKLIST_ENABLED=%s\n' "$GITRUN_GSR_COMMAND_BLACKLIST_ENABLED"
  printf 'GITRUN_GSR_COMMAND_BLACKLIST=%s\n' "$GITRUN_GSR_COMMAND_BLACKLIST"
  printf 'GITRUN_GSR_COMMAND_WHITELIST_ENABLED=%s\n' "$GITRUN_GSR_COMMAND_WHITELIST_ENABLED"
  printf 'GITRUN_GSR_COMMAND_WHITELIST=%s\n' "$GITRUN_GSR_COMMAND_WHITELIST"
  printf 'GITRUN_GSR_VIOLATION_ACTION=%s\n' "$GITRUN_GSR_VIOLATION_ACTION"
} > "$tmp_policy"
chown root:root "$tmp_policy"
chmod 0444 "$tmp_policy"
mv -f "$tmp_policy" "$GSR_POLICY_FILE"
trap - EXIT

GSR_EVENTS_FILE="${GSR_POLICY_DIR}/gsr-events.jsonl"
touch "$GSR_EVENTS_FILE"
chown runner:runner "$GSR_EVENTS_FILE"
chmod 0600 "$GSR_EVENTS_FILE"

mkdir -p "$SHARED_CACHE_DIR"
chown runner:runner "$SHARED_CACHE_DIR"

# GitDockRun can temporarily convert a completed runner container into a
# dock-only resource. The marker lives in the container writable layer, not
# in /run (which is tmpfs), so the state survives stop/start without starting
# another GitHub Actions runner.
if [[ ! -f "$DOCK_ONLY_MARKER" ]]; then
  : "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
fi

DOCK_ONLY_MARKER=/home/runner/.gitrun-dock-only
if [[ -f "$DOCK_ONLY_MARKER" ]]; then
  echo "gitrun-runner: starting in GitDockRun dock-only mode"
  unset RUNNER_TOKEN
  unset GITRUN_API_TOKEN
  unset GITRUN_GSR_COMMAND_POLICY_ENABLED
  unset GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED
  unset GITRUN_GSR_COMMAND_BLACKLIST_ENABLED
  unset GITRUN_GSR_COMMAND_BLACKLIST
  unset GITRUN_GSR_COMMAND_WHITELIST_ENABLED
  unset GITRUN_GSR_COMMAND_WHITELIST
  unset GITRUN_GSR_VIOLATION_ACTION
  exec sudo -u runner -E /bin/bash -c 'exec sleep infinity'
fi

if [[ -S /var/run/docker.sock ]]; then
  socket_gid="$(stat -c '%g' /var/run/docker.sock)"
  if [[ "$socket_gid" != "0" ]]; then
    if getent group "$socket_gid" >/dev/null 2>&1; then
      docker_socket_group="$(getent group "$socket_gid" | cut -d: -f1)"
    else
      docker_socket_group="docker-host"
      groupadd --gid "$socket_gid" "$docker_socket_group"
    fi
    usermod -aG "$docker_socket_group" runner
  fi
fi

sudo -u runner -E mkdir -p \
  "$SHARED_CACHE_DIR/cargo" \
  "$SHARED_CACHE_DIR/cargo-target" \
  "$SHARED_CACHE_DIR/pip" \
  "$SHARED_CACHE_DIR/npm"

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
unset GITRUN_GSR_COMMAND_POLICY_ENABLED
unset GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED
unset GITRUN_GSR_COMMAND_BLACKLIST_ENABLED
unset GITRUN_GSR_COMMAND_BLACKLIST
unset GITRUN_GSR_COMMAND_WHITELIST_ENABLED
unset GITRUN_GSR_COMMAND_WHITELIST
unset GITRUN_GSR_VIOLATION_ACTION

# GSR supervisor is intentionally the final PID 1 process. It keeps the
# CAP_SYS_PTRACE retained by the container solely for the supervisor and
# permanently drops the runner child to the unprivileged "runner" account
# before the Actions workload begins.
exec /usr/local/bin/gitrun-gsr-agent --supervise-runner
