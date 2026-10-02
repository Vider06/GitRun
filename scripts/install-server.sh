#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_BINARY="$ROOT/target/release/gitrun"
RECOVERY_BINARY="$ROOT/target/release/gitrun-recovery"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi

command -v docker >/dev/null 2>&1 || {
    echo "Docker is required." >&2
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
command -v cargo >/dev/null 2>&1 || {
    echo "Rust/Cargo is required to build GitRun from source." >&2
    exit 1
}
command -v rustc >/dev/null 2>&1 || {
    echo "rustc is required to build GitRun from source." >&2
    exit 1
}

cd "$ROOT"
echo "Using $(rustc --version)"
cargo build --locked --release -p gitrun-cli --bin gitrun
cargo build --locked --release -p gitrun-recovery --bin gitrun-recovery

if [[ ! -x "$BUILD_BINARY" || ! -x "$RECOVERY_BINARY" ]]; then
    echo "GitRun build completed but required binaries are missing or not executable." >&2
    exit 1
fi

install -m 0755 "$BUILD_BINARY" /usr/local/bin/gitrun
install -m 0755 "$RECOVERY_BINARY" /usr/local/bin/gitrun-recovery
exec /usr/local/bin/gitrun setup --terminal
