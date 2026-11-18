#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "APT mirror override must run as root" >&2
  exit 1
fi

# GitHub-hosted Ubuntu images can route the archive through Azure or a
# region-specific EC2 mirror list. Pin every Ubuntu archive entry to the
# canonical global archive before any apt-get update/install operation.
sources=(
  /etc/apt/apt-mirrors.txt
  /etc/apt/sources.list
  /etc/apt/sources.list.d/*.list
  /etc/apt/sources.list.d/*.sources
)

for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  sed -i -E \
    's#https?://(azure|[[:alnum:]-]+[.]ec2)[.]archive[.]ubuntu[.]com/ubuntu/?#https://archive.ubuntu.com/ubuntu/#g' \
    "$source"
done

# Fail closed: no Azure or regional EC2 archive endpoint may remain active.
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  if grep -Eqi '(azure|[[:alnum:]-]+[.]ec2)[.]archive[.]ubuntu[.]com' "$source"; then
    echo "Non-canonical Ubuntu archive mirror remains in $source" >&2
    exit 1
  fi
done

echo "APT Ubuntu archive mirror configuration:"
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  grep -E '(^|[[:space:]])(URIs:|deb |https?://)' "$source" || true
done
