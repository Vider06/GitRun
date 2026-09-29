#!/usr/bin/env bash
set -Eeuo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-}"
VERSION="${2:-$(git describe --tags --always --dirty)}"
[[ -n "$TARGET" ]] || TARGET="$(rustc -vV | awk '/host:/ {print $2}')"
HOST="$(rustc -vV | awk '/host:/ {print $2}')"
[[ "$TARGET" == "$HOST" ]] || { echo "Tauri local release builds must target the current Rust host ($HOST); got $TARGET" >&2; exit 1; }

cargo build --locked --release -p gitrun-cli --bin gitrun
command -v npm >/dev/null 2>&1 || { echo "npm is required to build the Tauri dashboard" >&2; exit 1; }
(
  cd crates/gitrun-dashboard-tauri
  npm install --ignore-scripts --no-audit --no-fund
  npm run tauri build -- --ci --no-bundle
)


mkdir -p dist/release/package
rm -f dist/release/GitRun-* dist/release/package/*
BINARY="gitrun"
case "$TARGET" in *windows*) BINARY="gitrun.exe";; esac
cp "target/release/$BINARY" dist/release/package/
DASHBOARD_BINARY="gitrun-dashboard-tauri"
case "$TARGET" in *windows*) DASHBOARD_BINARY="gitrun-dashboard-tauri.exe";; esac
cp "target/release/$DASHBOARD_BINARY" dist/release/package/
cp LICENSE README.md config/config.example.env dist/release/package/
ARCHIVE="dist/release/GitRun-$VERSION-$TARGET.tar.gz"
tar -C dist/release/package -czf "$ARCHIVE" .
SHA="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
printf '%s  %s\n' "$SHA" "$(basename "$ARCHIVE")" > "$ARCHIVE.sha256"
python3 - "$VERSION" "$TARGET" "$(basename "$ARCHIVE")" "$SHA" <<'PY'
import json, pathlib, subprocess, sys
version,target,file_name,sha256=sys.argv[1:]
manifest={"name":"GitRun","version":version,"git_commit":subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),"artifacts":[{"target":target,"file":file_name,"sha256":sha256}]}
pathlib.Path("dist/release/release-manifest.json").write_text(json.dumps(manifest,indent=2)+"\n",encoding="utf-8")
PY
echo "GitRun release build complete: $TARGET -> $ARCHIVE"
