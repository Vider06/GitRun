#!/bin/sh
set -eu

DOCKER_SOCKET=${GITRUN_DOCKER_SOCKET:-/var/run/docker.sock}
STATE_DIR=${GITRUN_STATE_DIR:-/var/lib/gitrun}
LOG_DIR=${GITRUN_LOG_DIR:-/var/log/gitrun}

if [ ! -S "$DOCKER_SOCKET" ]; then
    echo "gitrun-manager: Docker socket not found at $DOCKER_SOCKET" >&2
    exit 1
fi

socket_gid=$(stat -c '%g' "$DOCKER_SOCKET")
if [ -z "$socket_gid" ]; then
    echo "gitrun-manager: could not determine Docker socket GID" >&2
    exit 1
fi

if ! getent group "$socket_gid" >/dev/null 2>&1; then
    groupadd --gid "$socket_gid" gitrun-docker
fi

docker_group=$(getent group "$socket_gid" | cut -d: -f1)
usermod -aG "$docker_group" gitrun-manager

mkdir -p "$STATE_DIR" "$LOG_DIR"
chown gitrun-manager:gitrun-manager "$STATE_DIR" "$LOG_DIR"

exec setpriv     --reuid=gitrun-manager     --regid=gitrun-manager     --init-groups     /usr/local/bin/gitrun-autoscaler
