#!/usr/bin/env bash
set -Eeuo pipefail
REPO_URL="${GITRUN_REPO_URL:-https://github.com/Vider06/GitRun.git}"
INSTALL_DIR="${GITRUN_INSTALL_DIR:-$HOME/.gitrun}"
CONFIG_DIR="$INSTALL_DIR/config"; ENV_FILE="$CONFIG_DIR/gitrun.env"
if ! command -v docker >/dev/null 2>&1; then
  command -v brew >/dev/null 2>&1 || { echo "Homebrew is required for automatic Docker Desktop installation."; exit 1; }
  brew install --cask docker
fi
if ! docker info >/dev/null 2>&1; then open -a Docker; for _ in {1..60}; do docker info >/dev/null 2>&1 && break; sleep 2; done; fi
docker info >/dev/null 2>&1 || { echo "Docker Desktop did not become ready."; exit 1; }
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
mkdir -p "$CONFIG_DIR"
[[ -f "$ENV_FILE" ]] || { cp "$INSTALL_DIR/config/config.example.env" "$ENV_FILE"; chmod 600 "$ENV_FILE"; }
read -rp "GitHub token: " GITHUB_TOKEN
read -rp "Repositories (comma separated): " GITRUN_REPOSITORIES
[[ -n "$GITHUB_TOKEN" ]] || { echo "GitHub token is required." >&2; exit 1; }
[[ -n "$GITRUN_REPOSITORIES" ]] || { echo "At least one repository is required." >&2; exit 1; }
python3 - "$ENV_FILE" "$GITHUB_TOKEN" "$GITRUN_REPOSITORIES" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); lines=[]
for line in p.read_text().splitlines():
    if line.startswith("GITHUB_TOKEN="): line="GITHUB_TOKEN="+sys.argv[2]
    elif line.startswith("GITRUN_REPOSITORIES="): line="GITRUN_REPOSITORIES="+sys.argv[3]
    lines.append(line)
p.write_text("\n".join(lines)+"\n")
PY
export GITRUN_CONFIG_FILE="$ENV_FILE"
export GITRUN_STATE_DIR="$INSTALL_DIR/state"
export GITRUN_LOG_DIR="$INSTALL_DIR/logs"
mkdir -p "$GITRUN_STATE_DIR" "$GITRUN_LOG_DIR"
docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" config -q
docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" up -d --build
echo "GitRun installed and started."
