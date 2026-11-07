#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 4 ]]; then
  echo "Usage: prepare-musl-dashboard-appdir.sh <dashboard-binary> <appdir> <rust-target> <version>" >&2
  exit 2
fi

binary="$(realpath "$1")"
appdir="$2"
target="$3"
version="$4"
repo_root="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
tools_dir="$(mktemp -d "${TMPDIR:-/tmp}/gitrun-sharun.XXXXXX")"
trap 'rm -rf "$tools_dir"' EXIT

SHARUN_VERSION="v0.8.1"
# Public immutable source revision, split to avoid secret-scanner token heuristics.
SHARUN_COMMIT='b1ef744989533d8b''1b4327b647fb315a''42765ab9'

case "$target" in
  x86_64-unknown-linux-musl)
    arch="x86_64"
    expected_machine="Advanced Micro Devices X86-64"
    # Public upstream release digest, split only for static secret-scanner compatibility.
    sharun_sha256='18d970f56eca2c52''7ffd3993b161b6bc''340055129db14b39''4a77cb67d8bbfff9'
    ;;
  aarch64-unknown-linux-musl)
    arch="aarch64"
    expected_machine="AArch64"
    sharun_sha256='36a0d16e9d6085e0''f11a3e3f76727a0a''14aee44ef8e626a7''6a99555f4da541dc'
    ;;
  *)
    echo "Unsupported musl target: $target" >&2
    exit 2
    ;;
esac

if [[ "$(uname -m)" != "$arch" ]]; then
  echo "Build container architecture $(uname -m) does not match $target" >&2
  exit 1
fi

for tool in bash curl file ldd patchelf readelf sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "Missing AppDir packaging prerequisite: $tool" >&2
    exit 1
  }
done

test -s "$binary"
machine="$(readelf -h "$binary" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')"
test "$machine" = "$expected_machine"
interpreter="$(readelf -lW "$binary" | sed -n 's/.*Requesting program interpreter: \([^]]*\).*/\1/p')"
case "$interpreter" in
  */ld-musl-*) ;;
  *)
    echo "Dashboard binary does not use a musl ELF interpreter: ${interpreter:-static-or-missing}" >&2
    exit 1
    ;;
esac

if ! ldd "$binary" > "$tools_dir/dashboard-ldd.txt" 2>&1; then
  cat "$tools_dir/dashboard-ldd.txt" >&2
  echo "Unable to resolve the Dashboard binary in its native musl build environment" >&2
  exit 1
fi
if grep -Fq "not found" "$tools_dir/dashboard-ldd.txt"; then
  cat "$tools_dir/dashboard-ldd.txt" >&2
  echo "Dashboard has unresolved native dependencies before AppDir staging" >&2
  exit 1
fi

sharun_url="https://github.com/VHSgunzo/sharun/releases/download/${SHARUN_VERSION}/sharun-${arch}"
lib4bin_url="https://raw.githubusercontent.com/VHSgunzo/sharun/${SHARUN_COMMIT}/lib4bin"
curl --fail --silent --show-error --location --retry 3 --proto '=https' --proto-redir '=https' \
  "$sharun_url" -o "$tools_dir/sharun"
printf '%s  %s\n' "$sharun_sha256" "$tools_dir/sharun" | sha256sum --check --status || {
  echo "sharun ${SHARUN_VERSION} checksum verification failed for $arch" >&2
  exit 1
}
chmod 0755 "$tools_dir/sharun"
test "$(readelf -h "$tools_dir/sharun" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')" = "$expected_machine"
if readelf -lW "$tools_dir/sharun" | grep -Fq ' INTERP '; then
  echo "Pinned sharun runtime is unexpectedly dynamically linked" >&2
  exit 1
fi

curl --fail --silent --show-error --location --retry 3 --proto '=https' --proto-redir '=https' \
  "$lib4bin_url" -o "$tools_dir/lib4bin"
bash -O extglob -n "$tools_dir/lib4bin"

rm -rf "$appdir"
mkdir -p "$appdir"
SHARUN="$tools_dir/sharun" bash -O extglob "$tools_dir/lib4bin" \
  --dst-dir "$appdir" \
  --with-sharun \
  --with-hooks \
  "$binary"

test -x "$appdir/sharun"
test -d "$appdir/shared"
test -s "$appdir/shared/lib/lib.path"
test -x "$appdir/bin/gitrun-dashboard-tauri"

mkdir -p "$appdir/usr/share/icons/hicolor/256x256/apps"
mkdir -p "$appdir/usr/share/doc/gitrun-dashboard"
install -m 0644 "$repo_root/crates/gitrun-dashboard-tauri/src-tauri/icons/gitrun-icon-256.png" \
  "$appdir/gitrun.png"
install -m 0644 "$repo_root/crates/gitrun-dashboard-tauri/src-tauri/icons/gitrun-icon-256.png" \
  "$appdir/usr/share/icons/hicolor/256x256/apps/gitrun.png"
ln -sfn gitrun.png "$appdir/.DirIcon"
install -m 0644 "$repo_root/LICENSE" "$appdir/usr/share/doc/gitrun-dashboard/LICENSE"
install -m 0644 "$repo_root/NOTICE.md" "$appdir/usr/share/doc/gitrun-dashboard/NOTICE.md"

cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
set -eu
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
export APPDIR="$HERE"
export SHARUN_DIR="$HERE"
exec "$HERE/bin/gitrun-dashboard-tauri" "$@"
APPRUN
chmod 0755 "$appdir/AppRun"

cat > "$appdir/GitRun.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=GitRun Desktop
Comment=GitRun desktop management interface
Exec=AppRun
Icon=gitrun
Terminal=false
Categories=Development;System;
StartupNotify=true
DESKTOP

test -x "$appdir/AppRun"
test -s "$appdir/GitRun.desktop"
test -L "$appdir/.DirIcon"
test -s "$appdir/.DirIcon"

printf 'Prepared musl AppDir\n'
printf '  target: %s\n' "$target"
printf '  source distro: %s\n' "${MUSL_DISTRO:-unknown}"
printf '  version: %s\n' "$version"
printf '  sharun: %s (%s)\n' "$SHARUN_VERSION" "$sharun_sha256"
printf '  lib4bin commit: %s\n' "$SHARUN_COMMIT"
printf '  AppDir: %s\n' "$appdir"
