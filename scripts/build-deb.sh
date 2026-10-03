#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=${1:-}
BINARY=${2:-"$ROOT/target/release/gitrun"}
OUTPUT=${3:-gitrun.deb}

if [[ -z "$VERSION" ]]; then
  echo "usage: $0 <version> [binary] [output]" >&2
  exit 2
fi

if [[ "$VERSION" == v* ]]; then
  VERSION="${VERSION#v}"
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid GitRun version: $VERSION" >&2
  exit 2
fi

if [[ ! -x "$BINARY" ]]; then
  echo "GitRun binary is missing or not executable: $BINARY" >&2
  exit 1
fi

for command in dpkg-deb install; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command missing: $command" >&2
    exit 1
  }
done

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

root="$workdir/root"
install_root="$root/usr"
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/gitrun" "$root/usr/share/doc/gitrun" "$root/usr/share/applications"

install -m 0755 "$BINARY" "$root/usr/bin/gitrun"
install -m 0644 "$ROOT/version.txt" "$root/usr/share/gitrun/version.txt"
install -m 0644 "$ROOT/LICENSE" "$root/usr/share/doc/gitrun/LICENSE"
install -m 0644 "$ROOT/README.md" "$root/usr/share/doc/gitrun/README.md"
install -m 0644 "$ROOT/config/config.example.env" "$root/usr/share/doc/gitrun/config.example.env"
install -m 0644 "$ROOT/packaging/gitrun.desktop" "$root/usr/share/applications/gitrun.desktop"

for size in 32 64 128 256 512; do
  install -D -m 0644 "$ROOT/assets/gitrun-icon-$size.png" "$root/usr/share/icons/hicolor/${size}x${size}/apps/gitrun.png"
done

cat > "$root/DEBIAN/control" <<CONTROL
Package: gitrun
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Vider06
Depends: libc6, policykit-1, libwebkit2gtk-4.1-0, libgtk-3-0
Description: GitRun self-contained GitHub Actions runner manager
 GitRun provides the GitRun CLI, scheduler, recovery path and Tauri dashboard
 through a single executable.
CONTROL

rm -f "$OUTPUT"
dpkg-deb --build --root-owner-group "$root" "$OUTPUT" >/dev/null

echo "Built $OUTPUT"