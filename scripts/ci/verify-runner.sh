#!/usr/bin/env bash
set -euo pipefail

# shellcheck disable=SC1090,SC1091
source "$RUNNER_TEMP/ci-lib.sh"
chk_platform() {
  [ "$RUNNER_OS" = Linux ] || { echo "RUNNER_OS=$RUNNER_OS (Linux required)"; return 1; }
  [ "$(ci_arch)" != unknown ] || { echo "unsupported architecture $RUNNER_ARCH"; return 1; }
  echo "$RUNNER_OS/$RUNNER_ARCH on $RUNNER_NAME"
}
chk_dirs() {
  local d
  for d in "$GITHUB_WORKSPACE" "$RUNNER_TEMP" "$RUNNER_TOOL_CACHE"; do
    if [ ! -d "$d" ] || [ ! -w "$d" ]; then echo "$d is missing or not writable"; return 1; fi
  done
  echo "workspace, temp and tool cache are writable"
}
chk_exec_temp() {
  local f="$RUNNER_TEMP/exec-probe.sh"
  printf '#!/bin/sh\nexit 0\n' > "$f"; chmod +x "$f"
  if "$f"; then rm -f "$f"; echo "RUNNER_TEMP allows execution"; else rm -f "$f"; echo "RUNNER_TEMP is mounted noexec: rustup and build scripts will fail"; return 1; fi
}
chk_cmds() {
  local c miss=()
  for c in "$@"; do command -v "$c" >/dev/null 2>&1 || miss+=("$c"); done
  if [ "${#miss[@]}" -gt 0 ]; then echo "missing: ${miss[*]}"; return 1; fi
  echo "all present"
}

chk_host() {
  local code
  code="$(curl -sS -o /dev/null --connect-timeout 5 --max-time 10 -w '%{http_code}' "https://$1/" 2>&1)" || { echo "unreachable: $1"; return 1; }
  [ "$code" != 000 ] || { echo "unreachable: $1"; return 1; }
  echo "$1 → HTTP $code"
}
chk_repo_api() {
  curl -fsS --max-time 20 -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/${GITHUB_REPOSITORY}" -o /dev/null && echo "repository readable with the job token"
}
chk_rate() {
  curl -fsS --max-time 20 -H "Authorization: Bearer $GH_TOKEN" https://api.github.com/rate_limit | jq -r '.resources.core | "API quota \(.remaining)/\(.limit) left"'
}
chk_clock() {
  local d skew
  d="$(curl -sI --max-time 10 https://api.github.com | tr -d '\r' | awk -F': ' 'tolower($1)=="date"{print $2}')"
  [ -n "$d" ] || { echo "no Date header received"; return 1; }
  skew=$(( $(date +%s) - $(date -d "$d" +%s) ))
  [ "${skew#-}" -le 120 ] || { echo "clock skew ${skew}s (TLS and cache signatures may fail)"; return 1; }
  echo "clock skew ${skew}s"
}
chk_docker() {
  command -v docker >/dev/null 2>&1 || { echo "Docker CLI not installed: the docker job will be skipped on this runner"; return 1; }
  [ -S /var/run/docker.sock ] || { echo "Docker socket unavailable"; return 1; }
  docker info >/dev/null 2>&1 || { echo "Docker daemon not accessible to this user"; return 1; }
  docker --version
}
chk_ghenv() {
  local v
  for v in GITHUB_REPOSITORY GITHUB_SHA GITHUB_WORKSPACE RUNNER_NAME RUNNER_TEMP RUNNER_TOOL_CACHE; do
    [ -n "${!v:-}" ] || { echo "$v is empty"; return 1; }
  done
  echo "GitHub Actions environment is usable"
}
ci_check error "platform" chk_platform
ci_check error "writable directories" chk_dirs
ci_check error "RUNNER_TEMP executable" chk_exec_temp
ci_check error "required commands" chk_cmds bash git curl tar gzip xz sha256sum awk sed grep find

ci_check warn "build tools (Rust/Tauri jobs)" chk_cmds cc pkg-config make
ci_check warn "optional tools" chk_cmds docker
ci_check error "GitHub Actions environment" chk_ghenv
ci_check error "network: github.com" chk_host github.com
ci_check error "network: api.github.com" chk_host api.github.com
ci_check error "repository access" chk_repo_api
ci_check warn "API rate limit" chk_rate
ci_check warn "clock" chk_clock
for h in codeload.github.com release-assets.githubusercontent.com index.crates.io static.crates.io static.rust-lang.org sh.rustup.rs registry.npmjs.org download.docker.com pypi.org; do
  ci_check warn "network: $h" chk_host "$h"
done
ci_check warn "docker" chk_docker
ci_checks_finish "Runner environment — $RUNNER_NAME ($RUNNER_ARCH)"
