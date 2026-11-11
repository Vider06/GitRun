#!/usr/bin/env bash
set -euo pipefail

# ci-lib.sh is staged into RUNNER_TEMP by the shared CI bootstrap.
# shellcheck disable=SC1091
source "${RUNNER_TEMP:?}/ci-lib.sh"
ci_need_pkg libglib2.0-dev libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
ci_need_report
