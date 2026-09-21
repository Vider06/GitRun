#!/usr/bin/env bash
set -Eeuo pipefail
REPO_URL="${GITRUN_REPO_URL:-https://github.com/Vider06/GitRun.git}"
INSTALL_DIR="${GITRUN_INSTALL_DIR:-$HOME/.gitrun}"
CONFIG_DIR="$INSTALL_DIR/config"; ENV_FILE="$CONFIG_DIR/gitrun.env"
if ! command -v docker >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then sudo apt-get update && sudo apt-get install -y docker.io docker-compose-plugin git && sudo systemctl enable --now docker
  elif command -v dnf >/dev/null 2>&1; then sudo dnf install -y docker git && sudo systemctl enable --now docker
  else echo "Install Docker + Compose for this Linux distribution, then rerun."; exit 1; fi
fi
command -v git >/dev/null 2>&1 || { echo "Git is required."; exit 1; }
[[ -d "$INSTALL_DIR/.git" ]] && git -C "$INSTALL_DIR" pull --ff-only || git clone "$REPO_URL" "$INSTALL_DIR"
mkdir -p "$CONFIG_DIR" /var/lib/gitrun /var/log/gitrun
[[ -f "$ENV_FILE" ]] || { cp "$INSTALL_DIR/config/config.example.env" "$ENV_FILE"; chmod 600 "$ENV_FILE"; }
read -rp "GitHub token: " GITHUB_TOKEN
read -rp "Repositories (comma separated): " GITRUN_REPOSITORIES
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
export GITRUN_STATE_DIR=/var/lib/gitrun
export GITRUN_LOG_DIR=/var/log/gitrun
docker compose --env-file "$ENV_FILE" -f "$INSTALL_DIR/docker-compose.yml" up -d --build
echo "GitRun installed and started."
