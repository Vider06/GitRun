#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "APT mirror override must run as root" >&2
  exit 1
fi

# GitHub-hosted Ubuntu images may route archive.ubuntu.com through an
# Azure-specific mirror list. Rewrite both the mirror-list payload and any
# traditional/deb822 source definitions before the first apt-get update.
sources=(
  /etc/apt/apt-mirrors.txt
  /etc/apt/sources.list
  /etc/apt/sources.list.d/*.list
  /etc/apt/sources.list.d/*.sources
)

for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  sed -i -E \
    's#https?://azure[.]archive[.]ubuntu[.]com/ubuntu/?#http://archive.ubuntu.com/ubuntu/#g' \
    "$source"
done

# Fail closed: never continue to apt-get while an Azure archive endpoint
# remains in the active APT source configuration.
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  if grep -Eq 'azure[.]archive[.]ubuntu[.]com' "$source"; then
    echo "Azure Ubuntu archive mirror remains in $source" >&2
    exit 1
  fi
done

echo "APT Ubuntu archive mirror configuration:"
for source in "${sources[@]}"; do
  [ -f "$source" ] || continue
  case "$source" in
    /etc/apt/apt-mirrors.txt|/etc/apt/sources.list|/etc/apt/sources.list.d/*.list|/etc/apt/sources.list.d/*.sources)
      grep -E '(^|[[:space:]])(URIs:|deb |http://|https://)' "$source" || true
      ;;
  esac
done
