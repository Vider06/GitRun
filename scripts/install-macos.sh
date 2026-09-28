#!/usr/bin/env bash
set -Eeuo pipefail

REPO_URL="${GITRUN_REPO_URL:-https://github.com/Vider06/GitRun.git}"
INSTALL_DIR="${GITRUN_INSTALL_DIR:-$HOME/.gitrun}"
CONFIG_DIR="$INSTALL_DIR/config"
ENV_FILE="$CONFIG_DIR/gitrun.env"

if ! command -v docker >/dev/null 2>&1; then
  command -v brew >/dev/null 2>&1 || { echo "Homebrew is required for automatic Docker Desktop installation." >&2; exit 1; }
  brew install --cask docker
fi

if ! docker info >/dev/null 2>&1; then
  open -a Docker
  for _ in {1..60}; do
    docker info >/dev/null 2>&1 && break
    sleep 2
  done
fi
docker info >/dev/null 2>&1 || { echo "Docker Desktop did not become ready." >&2; exit 1; }
docker compose version >/dev/null 2>&1 || { echo "Docker Compose v2 is required." >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo "Git is required." >&2; exit 1; }
if ! command -v python3 >/dev/null 2>&1; then
  command -v brew >/dev/null 2>&1 || { echo "Python 3 is required." >&2; exit 1; }
  brew install python
fi

if [[ -d "$INSTALL_DIR/.git" ]]; then
  git -C "$INSTALL_DIR" pull --ff-only
else
  git clone "$REPO_URL" "$INSTALL_DIR"
fi

mkdir -p "$CONFIG_DIR" "$INSTALL_DIR/state" "$INSTALL_DIR/logs"
[[ -f "$ENV_FILE" ]] || { cp "$INSTALL_DIR/config/config.example.env" "$ENV_FILE"; chmod 600 "$ENV_FILE"; }

read -rsp "GitHub token: " GITHUB_TOKEN
echo
read -rp "Repositories (comma separated): " GITRUN_REPOSITORIES
[[ -n "$GITHUB_TOKEN" ]] || { echo "GitHub token is required." >&2; exit 1; }
[[ -n "$GITRUN_REPOSITORIES" ]] || { echo "At least one repository is required." >&2; exit 1; }

GITHUB_TOKEN="$GITHUB_TOKEN" GITRUN_REPOSITORIES="$GITRUN_REPOSITORIES" python3 - "$ENV_FILE" <<'PY'
from pathlib import Path
import os
import sys

p = Path(sys.argv[1])
token = os.environ["GITHUB_TOKEN"]
repositories = os.environ["GITRUN_REPOSITORIES"]
lines = []
for line in p.read_text(encoding="utf-8").splitlines():
    if line.startswith("GITHUB_TOKEN="):
        line = "GITHUB_TOKEN=" + token
    elif line.startswith("GITRUN_REPOSITORIES="):
        line = "GITRUN_REPOSITORIES=" + repositories
    lines.append(line)
p.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
unset GITHUB_TOKEN
chmod 600 "$ENV_FILE"

export GITRUN_CONFIG_FILE="$ENV_FILE"
export GITRUN_STATE_DIR="$INSTALL_DIR/state"
export GITRUN_LOG_DIR="$INSTALL_DIR/logs"

docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" --profile rust config -q
docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" up -d --build
echo "GitRun installed and started."
