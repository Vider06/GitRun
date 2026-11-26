#!/usr/bin/env bash
set -euo pipefail

# ci-lib.sh is staged into RUNNER_TEMP by the shared CI bootstrap.
# shellcheck disable=SC1091
source "${RUNNER_TEMP:?}/ci-lib.sh"

ci_need cc build-essential
ci_need_pkg libglib2.0-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf xdg-utils
ci_need convert imagemagick

shopt -s nullglob
icons=(crates/gitrun-dashboard-tauri/src-tauri/icons/*.png)
[ "${#icons[@]}" -gt 0 ] || { ci_err "No Tauri PNG icons found"; exit 1; }

for icon in "${icons[@]}"; do
  tmp="$(mktemp --suffix=.png)"
  convert "$icon" -alpha on -define png:color-type=6 "PNG32:$tmp"
  mv "$tmp" "$icon"
done

echo "${#icons[@]} Tauri PNG icons normalized in the CI checkout with ImageMagick"
ci_need_report
