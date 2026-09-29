#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

INSTALL_DIR="${GITRUN_INSTALL_DIR:-$HOME/.gitrun}"
CONFIG_DIR="$INSTALL_DIR/config"
ENV_FILE="$CONFIG_DIR/gitrun.env"
REPO_URL="https://github.com/Vider06/GitRun.git"

if ! command -v docker >/dev/null 2>&1; then
  command -v brew >/dev/null 2>&1 || {
    echo "Homebrew is required for automatic Docker Desktop installation." >&2
    exit 1
  }
  brew install --cask docker
fi

if ! docker info >/dev/null 2>&1; then
  open -a Docker
  for _ in {1..60}; do
    docker info >/dev/null 2>&1 && break
    sleep 2
  done
fi
docker info >/dev/null 2>&1 || {
  echo "Docker Desktop did not become ready." >&2
  exit 1
}
docker compose version >/dev/null 2>&1 || {
  echo "Docker Compose v2 is required." >&2
  exit 1
}
command -v git >/dev/null 2>&1 || {
  echo "Git is required." >&2
  exit 1
}

if [[ -d "$INSTALL_DIR/.git" ]]; then
  git -C "$INSTALL_DIR" pull --ff-only
else
  git clone "$REPO_URL" "$INSTALL_DIR"
fi

mkdir -p "$CONFIG_DIR" "$INSTALL_DIR/state" "$INSTALL_DIR/logs"
[[ -f "$ENV_FILE" ]] || {
  cp "$INSTALL_DIR/config/config.example.env" "$ENV_FILE"
  chmod 600 "$ENV_FILE"
}

read -rsp "GitHub token: " GITHUB_TOKEN
echo
read -rp "Repositories (comma separated): " GITRUN_REPOSITORIES

[[ -n "$GITHUB_TOKEN" ]] || {
  echo "GitHub token is required." >&2
  exit 1
}
[[ -n "$GITRUN_REPOSITORIES" ]] || {
  echo "At least one repository is required." >&2
  exit 1
}

if [[ ! "$GITRUN_REPOSITORIES" =~ ^[^,[:space:]/]+/[^,[:space:]/]+(,[^,[:space:]/]+/[^,[:space:]/]+)*$ ]]; then
  echo "Repositories must use owner/repository format, separated by commas." >&2
  exit 1
fi

temp_env="$(mktemp "$CONFIG_DIR/.gitrun.env.XXXXXX")"
cleanup_temp() {
  rm -f -- "$temp_env"
}
trap cleanup_temp EXIT

{
  found_token=false
  found_repositories=false
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      GITHUB_TOKEN=*)
        printf 'GITHUB_TOKEN=%s\n' "$GITHUB_TOKEN"
        found_token=true
        ;;
      GITRUN_REPOSITORIES=*)
        printf 'GITRUN_REPOSITORIES=%s\n' "$GITRUN_REPOSITORIES"
        found_repositories=true
        ;;
      *)
        printf '%s\n' "$line"
        ;;
    esac
  done < "$ENV_FILE"

  if [[ "$found_token" != true ]]; then
    printf 'GITHUB_TOKEN=%s\n' "$GITHUB_TOKEN"
  fi
  if [[ "$found_repositories" != true ]]; then
    printf 'GITRUN_REPOSITORIES=%s\n' "$GITRUN_REPOSITORIES"
  fi
} > "$temp_env"

chmod 600 "$temp_env"
mv -f -- "$temp_env" "$ENV_FILE"
trap - EXIT
unset GITHUB_TOKEN

export GITRUN_CONFIG_FILE="$ENV_FILE"
export GITRUN_STATE_DIR="$INSTALL_DIR/state"
export GITRUN_LOG_DIR="$INSTALL_DIR/logs"
export GITRUN_DOCKER_SOCKET=/var/run/docker.sock

docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" config -q
docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" up -d --build

container_health=""
for _ in {1..30}; do
  container_health="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' gitrun-manager 2>/dev/null || true)"
  case "$container_health" in
    healthy|running)
      echo "GitRun installed and started."
      exit 0
      ;;
    unhealthy|dead|exited)
      echo "GitRun manager failed health check (state: $container_health)." >&2
      docker logs --tail 50 gitrun-manager >&2 || true
      exit 1
      ;;
  esac
  sleep 2
done

echo "GitRun manager did not become ready (last state: ${container_health:-unknown})." >&2
docker logs --tail 50 gitrun-manager >&2 || true
exit 1
