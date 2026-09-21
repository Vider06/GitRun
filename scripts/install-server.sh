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

install -d /opt/gitrun /etc/gitrun /var/lib/gitrun /var/log/gitrun

cp -a "$ROOT/autoscaler" /opt/gitrun/
cp -a "$ROOT/docker" /opt/gitrun/
cp "$ROOT/docker-compose.yml" /opt/gitrun/
cp "$ROOT/systemd/gitrun.service" /etc/systemd/system/gitrun.service

if [[ ! -f /etc/gitrun/gitrun.env ]]; then
    install -m 0600 "$ROOT/config/config.example.env" /etc/gitrun/gitrun.env
else
    echo "Preserving existing /etc/gitrun/gitrun.env"
fi

docker build -t gitrun-runner:latest /opt/gitrun/docker/runner
docker build -t gitrun-manager:latest /opt/gitrun/docker/manager

systemctl daemon-reload
systemctl enable gitrun.service

echo
echo "GitRun server installation complete."
echo "Edit: /etc/gitrun/gitrun.env"
echo "Then run: sudo systemctl start gitrun"
echo "Health:    sudo gitrun doctor"
