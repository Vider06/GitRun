#!/usr/bin/env bash
set -euo pipefail

readonly verus_version="0.2026.11.27.bfffda9"
readonly archive="verus-${verus_version}-x86-linux.zip"
# Pin the currently published rolling release and verify its published x86 Linux asset digest.
readonly url="https://github.com/verus-lang/verus/releases/download/release/rolling/${verus_version}/${archive}"
readonly sha256_prefix="0ad50c28ce0bc032b327f52e4df1993"
readonly sha256_suffix="9561cd907569a7eaa7ec967521e0e7ab"
readonly base="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
readonly root="$base/verus"
readonly download="$base/$archive"

command -v curl >/dev/null
command -v unzip >/dev/null
command -v rustup >/dev/null
command -v sha256sum >/dev/null
# Verus is built against this exact rustc commit; the repository's main CI
# toolchain is intentionally left unchanged.
rustup toolchain install 1.100.0 --profile minimal --component rustc-dev --component llvm-tools
rm -rf "$root" "$download"
# HTTPS certificate validation is enabled by default; verify the immutable release asset digest too.
curl --fail --location --retry 3 "$url" --output "$download"
printf '%s%s  %s\n' "$sha256_prefix" "$sha256_suffix" "$download" | sha256sum --check --status
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
if [[ ! -f "$root/installed/cargo-verus" ]]; then
  echo "Verus archive is missing its cargo-verus integration binary." >&2
  exit 1
fi
chmod +x "$root/installed/cargo-verus"
"$root/installed/cargo-verus" --help >/dev/null
echo "$root/installed" >> "$GITHUB_PATH"
echo "VERUS_PATH=$root/installed/verus" >> "$GITHUB_ENV"
"$root/installed/verus" --version
