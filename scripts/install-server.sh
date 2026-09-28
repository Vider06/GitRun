#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "Run as root: sudo $0" >&2
    exit 1
fi
command -v docker >/dev/null 2>&1 || { echo "Docker is required." >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "Docker daemon is not reachable." >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "Rust/Cargo is required to build GitRun from source." >&2; exit 1; }

cd "$ROOT"
cargo build --locked --release -p gitrun-cli --bin gitrun
install -m 0755 target/release/gitrun /usr/local/bin/gitrun

exec /usr/local/bin/gitrun setup --terminal
