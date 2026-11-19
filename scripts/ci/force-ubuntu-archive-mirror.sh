#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "APT mirror override must run as root" >&2
  exit 1
fi

# Default to Canonical. The GARR endpoint is enabled only for the Tauri
# canary job through GITRUN_APT_MIRROR; do not change every CI job at once.
mirror="${GITRUN_APT_MIRROR:-https://archive.ubuntu.com/ubuntu/}"
case "$mirror" in
  https://archive.ubuntu.com/ubuntu|https://archive.ubuntu.com/ubuntu/)
    mirror="https://archive.ubuntu.com/ubuntu/"
    ;;
  https://ubuntu.mirror.garr.it/ubuntu-archive|https://ubuntu.mirror.garr.it/ubuntu-archive/)
    mirror="https://ubuntu.mirror.garr.it/ubuntu-archive/"
    ;;
  *)
    echo "Unsupported GITRUN_APT_MIRROR: $mirror" >&2
    exit 1
    ;;
esac

sources=(
  /etc/apt/apt-mirrors.txt
  /etc/apt/sources.list
  /etc/apt/sources.list.d/*.list
  /etc/apt/sources.list.d/*.sources
)

for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  sed -i -E \
    "s#https?://(([[:alnum:]-]+[.])?archive[.]ubuntu[.]com|security[.]ubuntu[.]com)/ubuntu/?#$mirror#g" \
    "$source"
done

# Fail closed: no Ubuntu archive host may bypass the selected mirror.
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  if grep -Eqi 'https?://(([[:alnum:]-]+[.])?archive[.]ubuntu[.]com|security[.]ubuntu[.]com)/ubuntu' "$source"; then
    echo "Ubuntu archive endpoint bypasses selected mirror in $source" >&2
    exit 1
  fi
done

echo "APT Ubuntu archive mirror: $mirror"
echo "APT Ubuntu archive mirror configuration:"
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  grep -E '(^|[[:space:]])(URIs:|deb |https?://)' "$source" || true
done
