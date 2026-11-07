#!/usr/bin/env bash
set -euo pipefail

readonly verus_version="0.2026.11.02.8bf68ee"
readonly archive="verus-${verus_version}-x86-linux.zip"
readonly url="https://github.com/verus-lang/verus/releases/download/release/${verus_version}/${archive}"
readonly base="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
readonly root="$base/verus"
readonly download="$base/$archive"

command -v curl >/dev/null
command -v unzip >/dev/null
command -v rustup >/dev/null
rm -rf "$root" "$download"
curl --fail --location --retry 3 --proto '=https' --tlsv1.2 "$url" --output "$download"
mkdir -p "$root"
unzip -q "$download" -d "$root"
if [[ -d "$root/verus-x86-linux" ]]; then
  mv "$root/verus-x86-linux" "$root/installed"
else
  echo "Unexpected Verus archive layout for ${verus_version}." >&2
  find "$root" -maxdepth 2 -type f -print >&2
  exit 1
fi
chmod +x "$root/installed/verus"
echo "$root/installed" >> "$GITHUB_PATH"
echo "VERUS_PATH=$root/installed/verus" >> "$GITHUB_ENV"
"$root/installed/verus" --version
