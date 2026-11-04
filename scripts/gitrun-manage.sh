#!/usr/bin/env bash
set -Eeuo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "$0")" && pwd)"
PYTHON="$(command -v python3 || true)"
if [[ -z "$PYTHON" ]]; then
    printf 'GitRun Logs helper requires Python 3.\n' >&2
    exit 127
fi
exec "$PYTHON" "$SCRIPT_DIR/gitrun-manage-linux.py" "$@"
