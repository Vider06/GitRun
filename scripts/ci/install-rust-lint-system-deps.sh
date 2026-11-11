#!/usr/bin/env bash
set -euo pipefail

source "${RUNNER_TEMP:?}/ci-lib.sh"
ci_need_pkg libglib2.0-dev
ci_need_report
