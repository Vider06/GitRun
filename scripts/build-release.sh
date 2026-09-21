#!/usr/bin/env bash
set -Eeuo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-}"
VERSION="${2:-$(git describe --tags --always --dirty)}"
[[ -n "$TARGET" ]] || TARGET="$(rustc -vV | awk '/host:/ {print $2}')"

cargo build --release -p gitrun-cli --target "$TARGET"

mkdir -p dist/release
BINARY="gitrun-rs"
case "$TARGET" in
  *windows*) BINARY="gitrun-rs.exe" ;;
esac
cp "target/$TARGET/release/$BINARY" "dist/release/$BINARY"
cp LICENSE README.md config/config.example.env dist/release/
python3 - "$VERSION" "$TARGET" <<'PY'
import json, pathlib, subprocess, sys
version, target = sys.argv[1:3]
manifest = {
    "name": "GitRun",
    "version": version,
    "git_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "artifacts": [{"target": target, "file": "gitrun-rs" if "windows" not in target else "gitrun-rs.exe", "sha256": ""}],
}
pathlib.Path("dist/release/release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
echo "GitRun release build complete: $TARGET"
