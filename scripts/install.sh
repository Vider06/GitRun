#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${GITRUN_PREFIX:-/usr/local}"
ETC_DIR="${GITRUN_ETC_DIR:-/etc/gitrun}"
BIN_DIR="$PREFIX/bin"

if [[ "$(id -u)" -ne 0 ]]; then
    echo "GitRun installer must run as root (use sudo)." >&2
    exit 1
fi

install -d "$BIN_DIR" "$ETC_DIR" /var/lib/gitrun /var/log/gitrun
install -m 0755 "$ROOT/bin/gitrun" "$BIN_DIR/gitrun"

if [[ ! -f "$ETC_DIR/gitrun.env" ]]; then
    install -m 0600 "$ROOT/config/config.example.env" "$ETC_DIR/gitrun.env"
    echo "Created $ETC_DIR/gitrun.env from the example configuration."
    echo "Edit it before connecting repositories or starting runners."
else
    echo "Preserved existing $ETC_DIR/gitrun.env"
fi

echo
echo "GitRun installed."
echo "Binary: $BIN_DIR/gitrun"
echo "Config: $ETC_DIR/gitrun.env"
echo
echo "Next:"
echo "  sudo gitrun doctor"
echo "  sudo gitrun overview"
