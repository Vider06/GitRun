#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-}"
VERSION="${2:-$(git describe --tags --always --dirty)}"
[[ -n "$TARGET" ]] || TARGET="$(rustc -vV | awk '/host:/ {print $2}')"
HOST="$(rustc -vV | awk '/host:/ {print $2}')"
[[ "$TARGET" == "$HOST" ]] || {
  echo "GitRun local release builds must target the current Rust host ($HOST); got $TARGET" >&2
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
test -x target/release/gitrun

mkdir -p dist/release/package
rm -f dist/release/GitRun-* dist/release/package/*
cp -- "target/release/gitrun" dist/release/package/
cp -- LICENSE README.md config/config.example.env version.txt dist/release/package/

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
