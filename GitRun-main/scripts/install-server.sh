#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi

command -v docker >/dev/null 2>&1 || {
    echo "Docker is required. Install Docker Engine first." >&2
    exit 1
}
docker info >/dev/null 2>&1 || {
    echo "Docker daemon is not reachable." >&2
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

install -d /opt/gitrun /etc/gitrun /var/lib/gitrun /var/log/gitrun /usr/local/bin
cp -a "$ROOT/autoscaler" /opt/gitrun/
cp -a "$ROOT/docker" /opt/gitrun/
cp "$ROOT/docker-compose.yml" /opt/gitrun/
install -m 0755 "$ROOT/bin/gitrun" /usr/local/bin/gitrun
cp "$ROOT/systemd/gitrun.service" /etc/systemd/system/gitrun.service

if [[ ! -f /etc/gitrun/gitrun.env ]]; then
    install -m 0600 "$ROOT/config/config.example.env" /etc/gitrun/gitrun.env
else
    echo "Preserving existing /etc/gitrun/gitrun.env"
fi

docker build -t gitrun-runner:latest /opt/gitrun/docker/runner
docker build -t gitrun-manager:latest /opt/gitrun/docker/manager

docker compose --env-file /etc/gitrun/gitrun.env -f /opt/gitrun/docker-compose.yml config -q

systemctl daemon-reload
systemctl enable gitrun.service

echo
echo "GitRun server installation complete."
echo "CLI:       /usr/local/bin/gitrun"
echo "Config:    /etc/gitrun/gitrun.env"
echo "State:     /var/lib/gitrun"
echo "Logs:      /var/log/gitrun"
echo
echo "Edit /etc/gitrun/gitrun.env, then:"
echo "  sudo systemctl start gitrun"
echo "  gitrun doctor"
echo "  gitrun overview"
