#!/usr/bin/env bash
set -euo pipefail

readonly verus_version="0.2026.11.27.835af2f"
readonly archive="verus-${verus_version}-x86-linux.zip"
# Pin the published rolling release and verify its published x86 Linux asset digest.
readonly url="https://github.com/verus-lang/verus/releases/download/release/rolling/${verus_version}/${archive}"
readonly sha256="9fe038c23eadc1d4c3da4943eb5f301f9be86e55f8d28d53ad0d7eb0764306f1"
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
printf '%s  %s\n' "$sha256" "$download" | sha256sum --check --status
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
