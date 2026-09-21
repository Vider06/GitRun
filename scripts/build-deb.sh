#!/usr/bin/env bash
set -euo pipefail

VERSION=${1:-}
BINARY=${2:-target/release/gitrun}
OUTPUT=${3:-gitrun.deb}

if [[ -z "$VERSION" ]]; then
  echo "usage: $0 <version> [binary] [output]" >&2
  exit 2
fi

if [[ "$VERSION" == v* ]]; then
  VERSION="${VERSION#v}"
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
mkdir -p \
  "$root/DEBIAN" \
  "$root/usr/local/bin" \
  "$root/usr/share/applications"

install -m 0755 "$BINARY" "$root/usr/local/bin/gitrun"

cat > "$root/DEBIAN/control" <<CONTROL
Package: gitrun
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Vider06
Depends: libc6, policykit-1
Description: GitRun self-contained GitHub Actions runner manager
 GitRun provides a graphical setup wizard and dashboard for managing
 GitHub Actions self-hosted runners.
CONTROL

cat > "$root/usr/share/applications/gitrun.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=GitRun
Comment=GitHub Actions runner manager
Exec=/usr/local/bin/gitrun dashboard
Terminal=false
Categories=Development;Utility;
DESKTOP

rm -f "$OUTPUT"
dpkg-deb --build --root-owner-group "$root" "$OUTPUT" >/dev/null

echo "Built $OUTPUT"
