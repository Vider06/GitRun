#!/usr/bin/env bash
set -euo pipefail

VERSION=${1:-}
BINARY=${2:-target/release/gitrun}
OUTPUT=${3:-gitrun.deb}
DASHBOARD_BINARY=${GITRUN_DASHBOARD_BINARY:-target/release/gitrun-dashboard-tauri}

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
if [[ ! -x "$DASHBOARD_BINARY" ]]; then
  echo "Tauri dashboard binary is missing or not executable: $DASHBOARD_BINARY" >&2
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
mkdir -p \
  "$root/DEBIAN" \
  "$root/usr/local/bin" \
  "$root/usr/bin" \
  "$root/usr/share/gitrun" \
  "$root/usr/share/applications"

install -m 0755 "$BINARY" "$root/usr/local/bin/gitrun"
install -m 0755 "$DASHBOARD_BINARY" "$root/usr/bin/gitrun-dashboard-tauri"

printf '%s\n' "$VERSION" > "$root/usr/share/gitrun/version.txt"

cat > "$root/DEBIAN/control" <<CONTROL
Package: gitrun
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Vider06
Depends: libc6, policykit-1, libwebkit2gtk-4.1-0, libgtk-3-0
Description: GitRun self-contained GitHub Actions runner manager
 GitRun provides a graphical setup wizard and dashboard for managing
 GitHub Actions self-hosted runners.
CONTROL

install -m 0644 packaging/gitrun.desktop "$root/usr/share/applications/gitrun.desktop"

rm -f "$OUTPUT"
dpkg-deb --build --root-owner-group "$root" "$OUTPUT" >/dev/null

echo "Built $OUTPUT"
