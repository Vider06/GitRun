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
case "$socket_gid" in
    ''|*[!0-9]*)
        echo "gitrun-manager: Docker socket GID is invalid: $socket_gid" >&2
        exit 1
        ;;
esac

if ! getent group "$socket_gid" >/dev/null 2>&1; then
    groupadd --gid "$socket_gid" gitrun-docker
fi

docker_group=$(getent group "$socket_gid" | cut -d: -f1)
if [ -z "$docker_group" ]; then
    echo "gitrun-manager: could not resolve Docker socket group $socket_gid" >&2
    exit 1
fi

usermod -aG "$docker_group" gitrun-manager

mkdir -p "$STATE_DIR" "$LOG_DIR"
chown gitrun-manager:gitrun-manager "$STATE_DIR" "$LOG_DIR"

exec setpriv     --reuid=gitrun-manager     --regid=gitrun-manager     --init-groups     --inh-caps=-all     --bounding-set=-all     --no-new-privs     /usr/local/bin/gitrun-autoscaler
