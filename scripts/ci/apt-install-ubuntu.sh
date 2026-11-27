#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "APT prerequisite installer must run as root" >&2
  exit 1
fi
if [ "$#" -eq 0 ]; then
  echo "Usage: $0 PACKAGE..." >&2
  exit 2
fi

# GitHub-hosted Ubuntu images may ship an optional Microsoft feed that can
# return 403 independently of Ubuntu archive health. Build an isolated source
# configuration for this transaction; do not mutate the runner's APT sources.
apt_config="$(mktemp -d)"
trap 'rm -rf "$apt_config"' EXIT
sourceparts="$apt_config/sources.list.d"
mkdir -p "$sourceparts"
sourcelist="$apt_config/sources.list"
: > "$sourcelist"

if [ -f /etc/apt/sources.list ]; then
  grep -Eiv 'packages[.]microsoft[.]com' /etc/apt/sources.list > "$sourcelist" || true
fi

shopt -s nullglob
for source in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  if grep -Eqi 'packages[.]microsoft[.]com' "$source"; then
    echo "Excluding optional Microsoft APT source: $source"
  else
    cp "$source" "$sourceparts/"
  fi
done
shopt -u nullglob

apt-get \
  -o Dir::Etc::sourcelist="$sourcelist" \
  -o Dir::Etc::sourceparts="$sourceparts/" \
  -o Acquire::http::Timeout=30 \
  -o Acquire::https::Timeout=30 \
  -o Acquire::Retries=2 \
  update

DEBIAN_FRONTEND=noninteractive apt-get \
  -o Acquire::http::Timeout=30 \
  -o Acquire::https::Timeout=30 \
  -o Acquire::Retries=2 \
  install -y --no-install-recommends "$@"
