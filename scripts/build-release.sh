#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-}"
VERSION="${2:-$(git describe --tags --always --dirty)}"
[[ -n "$TARGET" ]] || TARGET="$(rustc -vV | awk '/host:/ {print $2}')"
HOST="$(rustc -vV | awk '/host:/ {print $2}')"
[[ "$TARGET" == "$HOST" ]] || {
  echo "Tauri local release builds must target the current Rust host ($HOST); got $TARGET" >&2
  exit 1
}

if [[ "$VERSION" == v* ]]; then
  VERSION="${VERSION#v}"
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || {
  echo "Invalid GitRun release version: $VERSION" >&2
  exit 2
}

cargo build --locked --release -p gitrun-cli --bin gitrun
command -v npm >/dev/null 2>&1 || {
  echo "npm is required to build the Tauri dashboard" >&2
  exit 1
}
command -v python3 >/dev/null 2>&1 || {
  echo "python3 is required to prepare the Tauri dashboard version" >&2
  exit 1
}

TAURI_CONFIG="crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json"
TAURI_CONFIG_BACKUP="$(mktemp)"
cp -- "$TAURI_CONFIG" "$TAURI_CONFIG_BACKUP"
restore_tauri_config() {
  cp -- "$TAURI_CONFIG_BACKUP" "$TAURI_CONFIG"
  rm -f -- "$TAURI_CONFIG_BACKUP"
}
trap restore_tauri_config EXIT

python3 - "$VERSION" "$TAURI_CONFIG" <<'PY'
import json
import pathlib
import sys

version, config_path = sys.argv[1:]
path = pathlib.Path(config_path)
data = json.loads(path.read_text(encoding="utf-8"))
data["version"] = version
path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
PY

(
  cd crates/gitrun-dashboard-tauri
  npm install --ignore-scripts --no-audit --no-fund
  npm run tauri build -- --ci --no-bundle
)

mkdir -p dist/release/package
rm -f dist/release/GitRun-* dist/release/package/*
BINARY="gitrun"
case "$TARGET" in
  *windows*) BINARY="gitrun.exe" ;;
esac
DASHBOARD_BINARY="gitrun-dashboard-tauri"
case "$TARGET" in
  *windows*) DASHBOARD_BINARY="gitrun-dashboard-tauri.exe" ;;
esac

cp -- "target/release/$BINARY" dist/release/package/
cp -- "target/release/$DASHBOARD_BINARY" dist/release/package/
cp -- LICENSE README.md config/config.example.env dist/release/package/

ARCHIVE="dist/release/GitRun-$VERSION-$TARGET.tar.gz"
tar -C dist/release/package -czf "$ARCHIVE" .
if command -v sha256sum >/dev/null 2>&1; then
  SHA="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  SHA="$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
else
  echo "sha256sum or shasum is required to hash the release archive" >&2
  exit 1
fi
printf '%s  %s\n' "$SHA" "$(basename "$ARCHIVE")" > "$ARCHIVE.sha256"

python3 - "$VERSION" "$TARGET" "$(basename "$ARCHIVE")" "$SHA" <<'PY'
import json
import pathlib
import subprocess
import sys

version, target, file_name, sha256 = sys.argv[1:]
manifest = {
    "name": "GitRun",
    "version": version,
    "git_commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], text=True
    ).strip(),
    "artifacts": [
        {
            "target": target,
            "file": file_name,
            "sha256": sha256,
        }
    ],
}
pathlib.Path("dist/release/release-manifest.json").write_text(
    json.dumps(manifest, indent=2) + "\n",
    encoding="utf-8",
)
PY

echo "GitRun release build complete: $TARGET -> $ARCHIVE"
