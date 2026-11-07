#!/usr/bin/env bash
# shellcheck shell=bash
# ============================================================================
# GitRun CI helper library. Written to $RUNNER_TEMP by every job's Bootstrap
# step, then `source`d by the following steps. It never changes shell options.
# ============================================================================
CI_DIR="${RUNNER_TEMP:?RUNNER_TEMP is not set}/gitrun-ci"
CI_LOGS="$CI_DIR/logs"
CI_STEPS="$CI_DIR/steps.tsv"        # label <TAB> ok|fail|warn <TAB> seconds
CI_CACHES="$CI_DIR/caches.tsv"      # name <TAB> exact|partial|miss <TAB> size <TAB> key
CI_CHECKS="$CI_DIR/checks.tsv"      # level <TAB> ok|fail <TAB> name <TAB> detail
CI_ANNOT="$CI_DIR/annotations.txt"  # one bullet per error annotation (for the PR comment)
CI_TAILS="$CI_DIR/tails.txt"        # tail of every failed command
CI_DETAILS="$CI_DIR/details.md"      # job-specific detail sections
mkdir -p "$CI_LOGS"
touch "$CI_STEPS" "$CI_CACHES" "$CI_CHECKS" "$CI_ANNOT" "$CI_TAILS" "$CI_DETAILS"
export CI_DIR CI_LOGS CI_ANNOT CI_DETAILS
CI_MISSING=()

# ---------------------------------------------------------------- basics ----
ci_arch() { case "${RUNNER_ARCH:-$(uname -m)}" in X64|x86_64|amd64) echo x64 ;; ARM64|aarch64|arm64) echo arm64 ;; *) echo unknown ;; esac; }
ci_strip() { sed -E 's/\x1b\[[0-9;?]*[A-Za-z]//g'; }
ci_dur() { local s="${1:-0}"; if [ "$s" -ge 60 ]; then printf '%dm%02ds' $((s / 60)) $((s % 60)); else printf '%ds' "$s"; fi; }
ci_sum() { if [ -n "${CI_DETAILS:-}" ]; then cat >> "$CI_DETAILS"; elif [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat >> "$GITHUB_STEP_SUMMARY"; else cat > /dev/null; fi; }
ci_esc() { local s="$1"; s="${s//%/%25}"; s="${s//$'\r'/%0D}"; s="${s//$'\n'/%0A}"; printf '%s' "$s"; }
ci_escp() { local s; s="$(ci_esc "$1")"; s="${s//:/%3A}"; s="${s//,/%2C}"; printf '%s' "$s"; }

# ci_annot LEVEL FILE LINE COL TITLE MESSAGE  -> GitHub annotation (+ bullet for errors)
ci_annot() {
  local p=""
  [ -n "${2:-}" ] && p+="file=$(ci_escp "$2"),"
  [ -n "${3:-}" ] && p+="line=$3,"
  [ -n "${4:-}" ] && p+="col=$4,"
  [ -n "${5:-}" ] && p+="title=$(ci_escp "$5"),"
  if [ "${CI_SUPPRESS_ANNOTATIONS:-0}" != 1 ]; then
    printf '::%s %s::%s\n' "$1" "${p%,}" "$(ci_esc "${6:-}")"
  fi
  if [ "$1" = error ]; then
    printf '• %s%s %s\n' "${2:-}" "${3:+:$3}" "${5:+[$5] }${6:-}" >> "$CI_ANNOT"
  fi
  return 0
}
ci_err() { ci_annot error "" "" "" "" "$*"; }
ci_warn() { ci_annot warning "" "" "" "" "$*"; }
ci_note() { printf 'ℹ️  %s\n' "$*"; }

# ---------------------------------------------------------- run / retry ----
# ci_run LABEL CMD...   run with timing, own log group, tail on failure.
# CI_LEVEL=warning ci_run ...  => advisory: failure becomes a warning.
ci_run() {
  local label="$1"; shift
  local slug log t0 rc dur lvl="${CI_LEVEL:-error}" had_e=0
  slug="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '_')"
  log="$CI_LOGS/$slug.log"
  case $- in *e*) had_e=1 ;; esac
  t0=$(date +%s)
  echo "::group::▶ $label"
  printf '$'; printf ' %q' "$@"; echo
  set +e
  "$@" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  if [ "$had_e" = 1 ]; then set -e; fi
  dur=$(($(date +%s) - t0))
  echo "::endgroup::"
  if [ "$rc" -eq 0 ]; then
    printf '✅ %s (%s)\n' "$label" "$(ci_dur "$dur")"
    printf '%s\tok\t%s\n' "$label" "$dur" >> "$CI_STEPS"
    return 0
  fi
  echo "──── last 40 lines of: $label ────"
  tail -n 40 "$log" | ci_strip
  if [ "$lvl" = warning ]; then
    printf '⚠️  %s failed (exit %s) — advisory, not blocking\n' "$label" "$rc"
    ci_annot warning "" "" "" "$label" "advisory check failed (exit $rc) after $(ci_dur "$dur"); see log"
    printf '%s\twarn\t%s\n' "$label" "$dur" >> "$CI_STEPS"
    return 0
  fi
  printf '❌ %s failed (exit %s) after %s\n' "$label" "$rc" "$(ci_dur "$dur")"
  printf '::error title=%s::exit code %s after %s\n' "$(ci_escp "$label")" "$rc" "$(ci_dur "$dur")"
  { echo "### $label (exit $rc)"; tail -n 25 "$log" | ci_strip; echo; } >> "$CI_TAILS"
  printf '%s\tfail\t%s\n' "$label" "$dur" >> "$CI_STEPS"
  return "$rc"
}
ci_adv() { CI_LEVEL=warning ci_run "$@"; }

# ci_retry TRIES DELAY CMD...
ci_retry() {
  local n="$1" d="$2" i rc=1; shift 2
  for ((i = 1; i <= n; i++)); do
    if "$@"; then
      return 0
    else
      # Capture the failed command's status inside the else branch.
      # Reading $? after the completed if compound can report 0 when
      # no branch ran, falsely turning an exhausted retry into success.
      rc=$?
    fi
    ci_warn "attempt $i/$n failed (exit $rc): $*"
    if [ "$i" -lt "$n" ]; then sleep $((d * i)); fi
  done
  return "$rc"
}

# ci_download URL DEST [SHA256]
ci_download() {
  local url="$1" dest="$2" sha="${3:-}" optional="${4:-false}" t0 got
  local candidate="${dest}.partial.$$"
  t0=$(date +%s)
  rm -f "$candidate"
  if ! ci_retry 4 3 curl --fail --silent --show-error --location --connect-timeout 15 --max-time 600 -o "$candidate" "$url"; then
    rm -f "$candidate"
    if [ "$optional" = true ]; then
      ci_warn "optional prebuilt-tool download failed after retries: $url"
    else
      ci_err "download failed: $url"
    fi
    return 1
  fi
  if [ -n "$sha" ]; then
    got="$(sha256sum "$candidate" | cut -d' ' -f1)"
    if [ "$got" != "$sha" ]; then
      rm -f "$candidate"
      if [ "$optional" = true ]; then
        ci_warn "optional prebuilt-tool SHA-256 mismatch for $url (expected $sha, got $got)"
      else
        ci_err "sha256 mismatch for $url (expected $sha, got $got)"
      fi
      return 1
    fi
  fi
  mv "$candidate" "$dest"
  printf '⬇️  %s (%s bytes, %ss%s)\n' "${url##*/}" "$(stat -c %s "$dest")" "$(($(date +%s) - t0))" "${sha:+, sha256 ok}"
}

# ------------------------------------------------- tool install (user space) -
# ci_tool NAME VERSION URL SHA256 MODE [MEMBER]
#   MODE: bin (single file) | archive (extract, pick MEMBER by file name) | dir (extract everything)
# Installs into $RUNNER_TOOL_CACHE/gitrun-tools/NAME/VERSION-ARCH and adds it to PATH.
ci_tool() {
  local name="$1" ver="$2" url="$3" sha="$4" mode="$5" member="${6:-$1}" optional_download="${7:-false}"
  local dir tmp="$CI_DIR/dl-$name" found
  dir="${RUNNER_TOOL_CACHE:-$CI_DIR/tools}/gitrun-tools/$name/$ver-$(ci_arch)"
  if [ -f "$dir/.ok" ] && [ -x "$dir/$member" ]; then
    printf '✔ %s %s already installed (%s)\n' "$name" "$ver" "$dir"
  else
    rm -rf "$dir"; mkdir -p "$dir"
    ci_download "$url" "$tmp" "$sha" "$optional_download" || return 1
    case "$mode" in
      bin) install -m 0755 "$tmp" "$dir/$name" ;;
      dir) tar -xf "$tmp" -C "$dir" ;;
      archive)
        mkdir -p "$dir/.x"; tar -xf "$tmp" -C "$dir/.x"
        found="$(find "$dir/.x" -type f -name "$member" | head -n 1)"
        [ -n "$found" ] || { ci_err "$member not found inside ${url##*/}"; return 1; }
        install -m 0755 "$found" "$dir/$member"; rm -rf "$dir/.x" ;;
    esac
    rm -f "$tmp"; chmod +x "$dir/$member" 2>/dev/null || true
    printf '%s\n' "${sha:-unverified}" > "$dir/.ok"
    printf '✔ %s %s installed (%s)\n' "$name" "$ver" "$dir"
  fi
  export PATH="$dir:$PATH"
  [ -z "${GITHUB_PATH:-}" ] || echo "$dir" >> "$GITHUB_PATH"
}

# Pinned tools. Hashes were computed from the real release assets.
ci_ensure_jq() {
  command -v jq >/dev/null 2>&1 && return 0
  local v=1.8.2 u=https://github.com/jqlang/jq/releases/download/jq-1.8.2 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=b1c22172dd303f3be49e935aa56aa48a8b7a46e0bc838b4997d3bb451495870f; u="$u/jq-linux-amd64" ;;
    arm64) h=8b85c817833814ddca00a144c33705546355afccf0cf39b188f3cdb48b852309; u="$u/jq-linux-arm64" ;;
    *) ci_err "jq: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool jq "$v" "$u" "$h" bin
}
ci_ensure_shellcheck() {
  command -v shellcheck >/dev/null 2>&1 && return 0
  local v=v0.11.0 u=https://github.com/koalaman/shellcheck/releases/download/v0.11.0 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198; u="$u/shellcheck-v0.11.0.linux.x86_64.tar.xz" ;;
    arm64) h=12b331c1d2db6b9eb13cfca64306b1b157a86eb69db83023e261eaa7e7c14588; u="$u/shellcheck-v0.11.0.linux.aarch64.tar.xz" ;;
    *) ci_err "shellcheck: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool shellcheck "$v" "$u" "$h" archive
}
ci_ensure_actionlint() {
  command -v actionlint >/dev/null 2>&1 && return 0
  local v=1.7.12 u=https://github.com/rhysd/actionlint/releases/download/v1.7.12 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8; u="$u/actionlint_1.7.12_linux_amd64.tar.gz" ;;
    arm64) h=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6; u="$u/actionlint_1.7.12_linux_arm64.tar.gz" ;;
    *) ci_err "actionlint: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool actionlint "$v" "$u" "$h" archive
}
ci_ensure_hadolint() {
  command -v hadolint >/dev/null 2>&1 && return 0
  local v=2.15.1 u=https://github.com/hadolint/hadolint/releases/download/v2.15.1 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=c7187db94eeeeca956519a6af171adc31453941a1e777961f6e680f697c8c507; u="$u/hadolint-linux-x86_64" ;;
    arm64) h=f6198ef8090f404dbb771abfee086eb8c48ac177f30da7fd3510aca35b344b5d; u="$u/hadolint-linux-arm64" ;;
    *) ci_err "hadolint: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool hadolint "$v" "$u" "$h" bin
}
ci_ensure_pwsh() {
  command -v pwsh >/dev/null 2>&1 && return 0
  local v=7.6.6 u=https://github.com/PowerShell/PowerShell/releases/download/v7.6.6 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=ddbc4a2d113bbd46d283cfedcbcd117a70caefd7673f41f2b4e0000badf103bc; u="$u/powershell-7.6.6-linux-x64.tar.gz" ;;
    arm64) h=924829e54c983648f6f1419a2dc7f9433c861b2fb5bd57736ff096c24f133729; u="$u/powershell-7.6.6-linux-arm64.tar.gz" ;;
    *) ci_err "pwsh: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool pwsh "$v" "$u" "$h" dir
}
ci_ensure_cargo_deny() {
  command -v cargo-deny >/dev/null 2>&1 && cargo-deny --version 2>/dev/null | grep -q ' 0.20.2' && return 0
  local u=https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2 a h
  a="$(ci_arch)"
  case "$a" in
    x64) h=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f; u="$u/cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz" ;;
    arm64) h=995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d; u="$u/cargo-deny-0.20.2-aarch64-unknown-linux-musl.tar.gz" ;;
    *) ci_err "cargo-deny: unsupported architecture $a"; return 1 ;;
  esac
  ci_tool cargo-deny 0.20.2 "$u" "$h" archive
}
ci_ensure_cargo_audit() {
  command -v cargo-audit >/dev/null 2>&1 && cargo-audit --version 2>/dev/null | grep -q ' 0.22.2' && return 0
  local source_root
  source_root="${RUNNER_TOOL_CACHE:-$CI_DIR/tools}/gitrun-tools/cargo-audit/0.22.2-source-$(ci_arch)"
  if [ -x "$source_root/bin/cargo-audit" ] && "$source_root/bin/cargo-audit" --version 2>/dev/null | grep -q ' 0.22.2'; then
    export PATH="$source_root/bin:$PATH"
    [ -z "${GITHUB_PATH:-}" ] || echo "$source_root/bin" >> "$GITHUB_PATH"
    return 0
  fi

  if [ "$(ci_arch)" = x64 ]; then
    if ci_tool cargo-audit 0.22.2 \
      "https://github.com/rustsec/rustsec/releases/download/cargo-audit%2Fv0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz" \
      7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428 archive cargo-audit true; then
      return 0
    fi
    ci_warn "cargo-audit prebuilt release download unavailable; falling back to a pinned cargo install"
  else
    ci_note "no prebuilt cargo-audit for $(ci_arch): building from source (slow, cached afterwards)"
  fi

  mkdir -p "$source_root"
  if [ ! -x "$source_root/bin/cargo-audit" ]; then
    ci_run "cargo install cargo-audit (fallback)" cargo install --locked --version 0.22.2 --root "$source_root" cargo-audit || return 1
  fi
  export PATH="$source_root/bin:$PATH"
  [ -z "${GITHUB_PATH:-}" ] || echo "$source_root/bin" >> "$GITHUB_PATH"
}

# ------------------------------------------------ system prerequisites ----
ci_apt() {
  command -v apt-get >/dev/null 2>&1 || return 1
  local sudo="" started rc
  if [ "$(id -u)" != 0 ]; then
    if sudo -n true 2>/dev/null; then sudo="sudo -n"; else return 1; fi
  fi

  # Prefer the canonical Ubuntu archive instead of the Azure mirror that
  # has intermittently stalled on GitHub-hosted runners. Preserve Ubuntu's
  # suite/component configuration and leave security.ubuntu.com untouched.
  # This runs before update in every job that installs APT dependencies.
  if [ ! -f "$CI_DIR/apt-mirror-forced" ]; then
    if [ -f /etc/apt/apt-mirrors.txt ]; then
      $sudo sed -i -E 's#https?://azure[.]archive[.]ubuntu[.]com/ubuntu/?#http://archive.ubuntu.com/ubuntu/#g' /etc/apt/apt-mirrors.txt
      echo "APT mirror list after override:"
      $sudo cat /etc/apt/apt-mirrors.txt
    fi
    for source in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
      [ -f "$source" ] || continue
      $sudo sed -i -E 's#https?://azure[.]archive[.]ubuntu[.]com/ubuntu/?#http://archive.ubuntu.com/ubuntu/#g' "$source"
    done
    : > "$CI_DIR/apt-mirror-forced"
  fi

  # Bound network waits without changing the workflow/job timeout, and keep
  # the package-manager output visible in every job that uses this helper.
  if [ ! -f "$CI_DIR/apt-updated" ]; then
    started=$(date +%s)
    echo "[$(date -Is)] apt-get update started"
    if (set -o pipefail; $sudo apt-get \
      -o Acquire::http::Timeout=30 \
      -o Acquire::https::Timeout=30 \
      -o Acquire::Retries=2 \
      update 2>&1 | tee "$CI_LOGS/apt-update.log"); then
      : > "$CI_DIR/apt-updated"
      echo "[$(date -Is)] apt-get update finished in $(($(date +%s) - started))s"
    else
      rc=$?
      echo "[$(date -Is)] apt-get update failed after $(($(date +%s) - started))s (exit $rc); see $CI_LOGS/apt-update.log"
      return "$rc"
    fi
  fi

  started=$(date +%s)
  echo "[$(date -Is)] apt-get install started: $*"
  if (set -o pipefail; $sudo env DEBIAN_FRONTEND=noninteractive apt-get \
    -o Acquire::http::Timeout=30 \
    -o Acquire::https::Timeout=30 \
    -o Acquire::Retries=2 \
    install -y --no-install-recommends "$@" 2>&1 | tee "$CI_LOGS/apt.log"); then
    echo "[$(date -Is)] apt-get install finished in $(($(date +%s) - started))s"
    return 0
  else
    rc=$?
    echo "[$(date -Is)] apt-get install failed after $(($(date +%s) - started))s (exit $rc); see $CI_LOGS/apt.log"
    return "$rc"
  fi
}
# ci_need CMD [APT_PACKAGE]  -> installs the package when possible, otherwise records it
ci_need() {
  local c="$1" p="${2:-$1}"
  if command -v "$c" >/dev/null 2>&1; then return 0; fi
  if ci_apt "$p" && command -v "$c" >/dev/null 2>&1; then ci_note "installed $p (provides $c)"; return 0; fi
  CI_MISSING+=("command '$c' (apt package: $p)")
}
# ci_need_pkg PACKAGE...  -> dpkg libraries (headers etc.)
ci_need_pkg() {
  local p miss=()
  for p in "$@"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || miss+=("$p")
  done
  if [ "${#miss[@]}" -gt 0 ] && ci_apt "${miss[@]}"; then ci_note "installed: ${miss[*]}"; miss=(); fi
  for p in "${miss[@]}"; do CI_MISSING+=("package '$p'"); done
}
ci_need_report() {
  [ "${#CI_MISSING[@]}" -eq 0 ] && { echo "✅ system prerequisites present"; return 0; }
  local m pk=()
  for m in "${CI_MISSING[@]}"; do
    ci_err "missing on this runner image: $m"
    pk+=("$(printf '%s' "$m" | sed -E "s/.*(apt package: |package ')//; s/[')]//g")")
  done
  ci_err "install them on the runner image: sudo apt-get install -y ${pk[*]}"
  return 1
}

# ----------------------------------------------------------- diagnostics ----
ci_image_fp() {
  {
    # shellcheck source=/dev/null
    (. /etc/os-release 2>/dev/null; echo "${ID:-?}-${VERSION_ID:-?}")
    getconf GNU_LIBC_VERSION 2>/dev/null
    cc --version 2>/dev/null | head -n 1
    uname -m
  } | sha256sum | cut -c1-8
}
ci_diag() {
  local ws="${GITHUB_WORKSPACE:-.}" avail_kb inode_pct mem_mb
  echo "::group::Runner diagnostics"
  echo "runner: ${RUNNER_NAME:-?} (${RUNNER_OS:-?}/${RUNNER_ARCH:-?})  job: ${GITHUB_JOB:-?}  sha: ${GITHUB_SHA:0:12}"
  # shellcheck source=/dev/null
  echo "image : $( (. /etc/os-release 2>/dev/null; echo "${PRETTY_NAME:-unknown}") ) | $(getconf GNU_LIBC_VERSION 2>/dev/null) | fingerprint $(ci_image_fp)"
  echo "kernel: $(uname -srm)  cpus: $(nproc 2>/dev/null || echo ?)  load: $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
  df -hT "$ws" "$RUNNER_TEMP" 2>/dev/null || true
  df -i "$ws" "$RUNNER_TEMP" 2>/dev/null || true
  free -m 2>/dev/null || true
  if [ -r /sys/fs/cgroup/memory.max ]; then
    echo "cgroup memory: $(cat /sys/fs/cgroup/memory.current 2>/dev/null || echo unknown) / $(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo unknown) bytes"
  elif [ -r /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then
    echo "cgroup memory limit: $(cat /sys/fs/cgroup/memory/memory.limit_in_bytes)"
  fi
  if command -v docker >/dev/null 2>&1; then
    if timeout 5s docker info >/dev/null 2>&1; then echo "docker daemon: available"; else echo "docker daemon: installed but unavailable"; fi
  else
    echo "docker daemon: not installed"
  fi
  if [ "${CI_VERBOSE:-0}" = 1 ]; then echo "--- environment (secrets filtered) ---"; env | grep -viE 'token|secret|password|key' | sort; fi
  echo "::endgroup::"
  avail_kb="$(df -Pk "$ws" | awk 'NR==2{print $4}')"
  temp_avail_kb="$(df -Pk "$RUNNER_TEMP" | awk 'NR==2{print $4}')"
  inode_pct="$(df -Pi "$ws" | awk 'NR==2{gsub("%","",$5); print $5}')"
  mem_mb="$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 99999)"
  if [ "${avail_kb:-0}" -lt 1048576 ] || [ "${temp_avail_kb:-0}" -lt 1048576 ]; then
    ci_err "disk almost full on the runner: workspace=$((avail_kb / 1024)) MB, runner-temp=$((temp_avail_kb / 1024)) MB free (need >= 1 GB on both). Clean _work/ and Docker images on $RUNNER_NAME."
    return 1
  fi
  if [ "${avail_kb:-0}" -lt 5242880 ] || [ "${temp_avail_kb:-0}" -lt 5242880 ]; then
    ci_warn "low disk space on $RUNNER_NAME: workspace=$((avail_kb / 1024)) MB, runner-temp=$((temp_avail_kb / 1024)) MB"
  fi
  if [ "${inode_pct:-0}" -ge 90 ]; then ci_warn "inodes ${inode_pct}% used on $RUNNER_NAME"; fi
  if [ "${mem_mb:-99999}" -lt 1500 ]; then ci_warn "low memory: ${mem_mb} MB available on $RUNNER_NAME"; fi
  return 0
}

# ci_check LEVEL NAME CMD...   (error|warn) collects results, never exits
ci_check() {
  local lvl="$1" name="$2" out rc started elapsed result; shift 2
  started="$(date +%s)"
  if out="$("$@" 2>&1)"; then
    elapsed=$(( $(date +%s) - started ))
    printf '✅ %s%s\n' "$name" "${out:+ — ${out%%$'\n'*}}"
    printf '%s\tok\t%s\t%s\n' "$lvl" "$name" "${out%%$'\n'*}" >> "$CI_CHECKS"
    result=ok
  else
    rc=$?
    elapsed=$(( $(date +%s) - started ))
    out="${out%%$'\n'*}"
    if [ "$lvl" = error ]; then
      printf '❌ %s — %s\n' "$name" "${out:-exit $rc}"
      ci_annot error "" "" "" "$name" "${out:-exit $rc}"
      result=fail
    else
      printf '⚠️  %s — %s\n' "$name" "${out:-exit $rc}"
      ci_annot warning "" "" "" "$name" "${out:-exit $rc}"
      result=warn
    fi
    printf '%s\tfail\t%s\t%s\n' "$lvl" "$name" "${out:-exit $rc}" >> "$CI_CHECKS"
  fi
  printf '%s\t%s\t%s\n' "$name" "$result" "$elapsed" >> "$CI_STEPS"
  return 0
}
# ci_checks_finish TITLE -> summary table, returns 1 if any error-level check failed
ci_checks_finish() {
  local lvl st name detail bad=0
  {
    echo "### $1"; echo; echo "| Check | Result | Detail |"; echo "|---|---|---|"
    while IFS=$'\t' read -r lvl st name detail; do
      if [ "$st" = ok ]; then echo "| $name | ✅ | ${detail//|/\\|} |"
      elif [ "$lvl" = error ]; then echo "| $name | ❌ | ${detail//|/\\|} |"; bad=1
      else echo "| $name | ⚠️ | ${detail//|/\\|} |"; fi
    done < "$CI_CHECKS"
    echo
  } | ci_sum
  while IFS=$'\t' read -r lvl st name detail; do if [ "$st" = fail ] && [ "$lvl" = error ]; then bad=1; fi; done < "$CI_CHECKS"
  return "$bad"
}

# ci_gate LEVEL NAME CMD...  like ci_check, but the output is NOT captured: use it for
# functions that print their own annotations. CMD returns non-zero when it found problems.
ci_gate() {
  local lvl="$1" name="$2" rc icon="❌" started elapsed result; shift 2
  started="$(date +%s)"
  if "$@"; then
    elapsed=$(( $(date +%s) - started ))
    printf '✅ %s\n' "$name"
    printf '%s\tok\t%s\t\n' "$lvl" "$name" >> "$CI_CHECKS"
    result=ok
  else
    rc=$?
    elapsed=$(( $(date +%s) - started ))
    if [ "$lvl" != error ]; then icon="⚠️"; result=warn; else result=fail; fi
    printf '%s %s (exit %s)\n' "$icon" "$name" "$rc"
    printf '%s\tfail\t%s\tsee annotations / log\n' "$lvl" "$name" >> "$CI_CHECKS"
  fi
  printf '%s\t%s\t%s\n' "$name" "$result" "$elapsed" >> "$CI_STEPS"
  return 0
}

# ------------------------------------------------------------ cache logs ----
# ci_cache_note NAME PRIMARY_KEY MATCHED_KEY PATH...
ci_cache_note() {
  local name="$1" primary="$2" matched="$3" res size="-"; shift 3
  if [ -z "$matched" ]; then res=miss; elif [ "$matched" = "$primary" ]; then res=exact; else res=partial; fi
  if [ "$res" != miss ] && [ "$#" -gt 0 ]; then size="$(du -sch "$@" 2>/dev/null | tail -n 1 | cut -f1)"; fi
  printf '🗄️  cache %-22s %-8s %s\n' "$name" "$res" "$size"
  case "$res" in
    exact) echo "    ↳ identical inputs: nothing to rebuild" ;;
    partial) echo "    ↳ restored from an older key (${matched}); only what changed gets rebuilt" ;;
    miss) echo "    ↳ no usable cache: cold start, everything is built/downloaded" ;;
  esac
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$res" "$size" "$primary" "$matched" >> "$CI_CACHES"
}

# ---------------------------------------------------- output parsers (awk) ----
# pe(): escape a property value (file/title); me(): escape a message.
CI_AWK_FN='function pe(s) { gsub(/%/, "%25", s); gsub(/:/, "%3A", s); gsub(/,/, "%2C", s); return s }
function me(s) { gsub(/%/, "%25", s); gsub(/\r/, "", s); return s }
'

# All parsers pass the input through unchanged and add ::error/::warning lines.
# cargo --message-format=json-diagnostic-rendered-ansi  (needs jq)
ci_cargo_json() {
  jq -rR --arg ws "${GITHUB_WORKSPACE:-}/" '
    (fromjson? // empty) | select(.reason == "compiler-message") | .message as $m
    | (if ($m.rendered // "") != "" then ($m.rendered | rtrimstr("\n")) else empty end),
      ( ([$m.spans[]? | select(.is_primary)] | first) as $s
        | select($s != null and ($m.level == "error" or $m.level == "warning"))
        | "::\($m.level) file=\($s.file_name | ltrimstr($ws)),line=\($s.line_start),col=\($s.column_start),endLine=\($s.line_end),title=\((($m.code.code // $m.level) | gsub(":"; "%3A")))::\($m.message | gsub("%"; "%25") | gsub("\n"; "%0A"))" )
  ' | awk -v max="${CI_MAX_ANNOT:-40}" -v out="$CI_ANNOT" '
    /^::(error|warning) / {
      if (seen[$0]++) next
      if (++n > max) { skipped++; next }
      print
      if ($0 ~ /^::error /) {
        i = index(substr($0, 3), "::"); h = substr($0, 3, i - 1); m = substr($0, 3 + i + 1)
        match(h, /file=[^,]*/); f = substr(h, RSTART + 5, RLENGTH - 5)
        match(h, /line=[0-9]+/); l = substr(h, RSTART + 5, RLENGTH - 5)
        print "• " f ":" l " " m >> out
      }
      next
    }
    { print }
    END { if (skipped > 0) printf "::notice::%d more diagnostics not annotated (limit %d); see the log\n", skipped, max }
  '
}
# rustfmt --check
ci_awk_fmt() {
  awk -v ws="${GITHUB_WORKSPACE:-}/" -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    /^Diff in / {
      l = $0; sub(/^Diff in /, "", l); sub(/:$/, "", l)
      if (match(l, / at line [0-9]+$/)) { ln = substr(l, RSTART + 9); f = substr(l, 1, RSTART - 1) }
      else { ln = l; sub(/^.*:/, "", ln); f = l; sub(/:[0-9]+$/, "", f) }
      sub("^" ws, "", f)
      if (!(f ":" ln in seen)) {
        seen[f ":" ln] = 1; files[f] = 1
        printf "::error file=%s,line=%s,title=rustfmt::formatting differs here — run `cargo fmt --all`\n", pe(f), ln
        print "• " f ":" ln " [rustfmt] formatting differs" >> out
      }
    }'
}
# libtest output: failed tests with panic location, plus totals
ci_awk_tests() {
  awk -v ws="${GITHUB_WORKSPACE:-}/" -v out="$CI_ANNOT" -v tot="$CI_DIR/test-totals.txt" "$CI_AWK_FN"'
    { print }
    /^test .* \.\.\. FAILED$/ { t = $0; sub(/^test /, "", t); sub(/ \.\.\. FAILED$/, "", t); failed[t] = 1 }
    /^thread .* panicked at / {
      l = $0; sub(/^thread .* panicked at /, "", l); sub(/:$/, "", l)
      th = $0; sub(/^thread \x27/, "", th); sub(/\x27 panicked.*$/, "", th)
      n = split(l, p, ":"); col = p[n]; line = p[n - 1]; f = p[1]; for (i = 2; i < n - 1; i++) f = f ":" p[i]
      sub("^" ws, "", f); want = 1; wth = th; wf = f; wl = line; wc = col; next
    }
    want && NF > 0 {
      msg = $0; sub(/^[ \t]+/, "", msg); want = 0
      printf "::error file=%s,line=%s,col=%s,title=%s::%s\n", pe(wf), wl, wc, pe("test failed: " wth), me(msg)
      print "• " wf ":" wl " [test failed: " wth "] " msg >> out
    }
    /^test result: / {
      for (i = 1; i <= NF; i++) { if ($i == "passed;") pa += $(i - 1); if ($i == "failed;") fa += $(i - 1); if ($i == "ignored;") ig += $(i - 1) }
    }
    END { printf "passed=%d failed=%d ignored=%d\n", pa, fa, ig > tot }'
}
# gcc style: file:line:col: level: message [SCxxxx]   (shellcheck -f gcc)
ci_awk_gcc() {
  awk -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    match($0, /^[^:]+:[0-9]+:[0-9]+: (error|warning|note): /) {
      split($0, h, ": "); loc = h[1]; lvl = h[2]
      m = $0; sub(/^[^:]+:[0-9]+:[0-9]+: (error|warning|note): /, "", m)
      n = split(loc, p, ":"); f = p[1]; ln = p[2]; col = p[3]
      code = ""; if (match(m, / \[SC[0-9]+\]$/)) { code = substr(m, RSTART + 2, RLENGTH - 3); m = substr(m, 1, RSTART - 1) }
      al = (lvl == "note") ? "notice" : lvl
      printf "::%s file=%s,line=%s,col=%s,title=%s::%s\n", al, pe(f), ln, col, pe(code == "" ? "shellcheck" : code), me(m)
      if (al == "error") print "• " f ":" ln " [" code "] " m >> out
    }'
}
# hadolint (tty, no color): Dockerfile:3 DL3006 warning: message
ci_awk_hadolint() {
  awk -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    match($0, /^[^ ]+:[0-9]+ [A-Z]+[0-9]+ (error|warning|info|style): /) {
      split($0, h, " "); loc = h[1]; code = h[2]; lvl = h[3]; sub(/:$/, "", lvl)
      m = $0; sub(/^[^ ]+:[0-9]+ [A-Z]+[0-9]+ (error|warning|info|style): /, "", m)
      n = split(loc, p, ":"); ln = p[n]; f = p[1]; for (i = 2; i < n; i++) f = f ":" p[i]
      al = (lvl == "error") ? "error" : (lvl == "warning" ? "warning" : "notice")
      printf "::%s file=%s,line=%s,title=%s::%s\n", al, pe(f), ln, pe(code), me(m)
      if (al == "error") print "• " f ":" ln " [" code "] " m >> out
    }'
}
# git diff --check:  file:line: trailing whitespace.   LEVEL via -v lvl=
ci_awk_diffcheck() {
  awk -v lvl="${1:-warning}" -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    match($0, /^[^: ][^:]*:[0-9]+: /) {
      split($0, p, ":"); f = p[1]; ln = p[2]; m = $0; sub(/^[^:]+:[0-9]+: /, "", m)
      printf "::%s file=%s,line=%s,title=whitespace::%s\n", lvl, pe(f), ln, me(m)
      if (lvl == "error") print "• " f ":" ln " [whitespace] " m >> out
    }'
}
# cargo audit (human output) -> annotations on Cargo.lock.
# A block with a "Warning:" line (unmaintained/unsound/yanked) is a warning, any other block is a vulnerability.
ci_awk_audit() {
  awk -v out="$CI_ANNOT" "$CI_AWK_FN"'
    function flush(   al, msg) {
      if (crate != "") {
        al = (kind != "") ? "warning" : "error"
        msg = crate " " ver ": " title (kind != "" ? " (" kind ")" : "") (sol != "" ? " — " sol : "")
        printf "::%s file=Cargo.lock,title=%s::%s\n", al, pe(id == "" ? "advisory" : id), me(msg)
        if (al == "error") print "• Cargo.lock [" id "] " msg >> out
      }
      crate = ver = title = id = sol = kind = ""
    }
    { print }
    /^Crate: / { flush(); crate = $2 }
    /^Version: / { ver = $2 }
    /^Warning: / { kind = $2 }
    /^Title: / { title = $0; sub(/^Title: +/, "", title) }
    /^ID: / { id = $2 }
    /^Solution: / { sol = $0; sub(/^Solution: +/, "", sol) }
    /^(error|warning): [0-9]+ / { flush() }
    END { flush() }'
}
# npm audit (text) -> annotations on package.json.  Arg 1: path of that package.json
ci_awk_npm() {
  awk -v pf="${1:-package.json}" -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    /^Severity: / {
      sev = $2; pkg = prev
      al = (sev == "high" || sev == "critical") ? "error" : "warning"
      if ((getline t) > 0) print t
      printf "::%s file=%s,title=%s::%s — %s\n", al, pe(pf), pe("npm audit " sev), me(pkg), me(t)
      if (al == "error") print "• " pf " [npm audit " sev "] " pkg " — " t >> out
    }
    { prev = $0 }'
}

# node --check errors:  /abs/file.js:12 ... SyntaxError: msg   (-v ws via GITHUB_WORKSPACE)
ci_awk_node() {
  awk -v ws="${GITHUB_WORKSPACE:-}/" -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    /^[^ ]+\.[cm]?js:[0-9]+$/ { f = $0; sub(/:[0-9]+$/, "", f); l = $0; sub(/^.*:/, "", l); sub("^" ws, "", f); pending = 1; next }
    pending && /^[A-Za-z]*Error: / {
      m = $0; sub(/^[A-Za-z]*Error: /, "", m)
      printf "::error file=%s,line=%s,title=node --check::%s\n", pe(f), l, me(m)
      print "• " f ":" l " [node --check] " m >> out
      pending = 0
    }'
}
# bash -n errors:  file: line 3: syntax error ...
ci_awk_bashn() {
  awk -v out="$CI_ANNOT" "$CI_AWK_FN"'
    { print }
    match($0, /^[^:]+: line [0-9]+: /) {
      f = $0; sub(/: line [0-9]+: .*$/, "", f); l = $0; sub(/^[^:]+: line /, "", l); sub(/:.*$/, "", l)
      m = $0; sub(/^[^:]+: line [0-9]+: /, "", m)
      printf "::error file=%s,line=%s,title=bash -n::%s\n", pe(f), l, me(m)
      print "• " f ":" l " [bash -n] " m >> out
    }'
}

# ci_tap: pass-through that copies every ::error annotation into the error bullet list.
ci_tap() {
  awk -v out="$CI_ANNOT" '
    { print }
    /^::error / {
      i = index(substr($0, 3), "::"); h = substr($0, 3, i - 1); m = substr($0, 3 + i + 1); f = ""; l = ""
      if (match(h, /file=[^,]*/)) f = substr(h, RSTART + 5, RLENGTH - 5)
      if (match(h, /line=[0-9]+/)) l = substr(h, RSTART + 5, RLENGTH - 5)
      print "• " f (l != "" ? ":" l : "") " " m >> out
    }'
}

# ------------------------------------------------------------ job report ----
ci_report() {
  local t0 secs lbl st d res sz primary matched line started_at generated_at status status_label report
  t0="$(cat "$CI_DIR/t0" 2>/dev/null || date +%s)"
  secs=$(( $(date +%s) - t0 ))
  started_at="$(cat "$CI_DIR/started_at" 2>/dev/null || printf 'unavailable')"
  generated_at="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
  status="${GITRUN_JOB_STATUS:-unknown}"
  case "$status" in
    success) status_label="success" ;;
    failure) status_label="failure" ;;
    cancelled) status_label="cancelled" ;;
    *) status_label="unavailable ($status)" ;;
  esac
  report="$CI_DIR/report.md"
  {
    echo "# GitRun CI job report"
    echo
    echo "| Field | Runtime value |"
    echo "|---|---|"
    echo "| Job | ${GITHUB_JOB:-job} |"
    echo "| Result | **$status_label** |"
    echo "| Execution duration | $(ci_dur "$secs") |"
    echo "| Measurement started (UTC) | $started_at |"
    echo "| Report generated (UTC) | $generated_at |"
    echo "| Runner | ${RUNNER_NAME:-unknown} (${RUNNER_OS:-unknown}/${RUNNER_ARCH:-unknown}) |"
    echo "| Commit | ${GITHUB_SHA:0:12} |"
    echo "| Run | [${GITHUB_RUN_ID:-unknown}](${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown}/actions/runs/${GITHUB_RUN_ID:-}) |"
    echo
    echo "_Duration is measured from completion of the shared CI helper bootstrap and runner diagnostics. Runner queue time and checkout are excluded. Skipped jobs cannot write a job-local summary; their status is reported by the workflow-level verification summary._"
    echo
    echo "## Step results and timings"
    echo
    if [ -s "$CI_STEPS" ]; then
      echo "| Step / check | Result | Time |"
      echo "|---|---|---:|"
      while IFS=$'\t' read -r lbl st d; do
        case "$st" in
          ok) echo "| ${lbl//|/\\|} | success | $(ci_dur "$d") |" ;;
          warn) echo "| ${lbl//|/\\|} | warning | $(ci_dur "$d") |" ;;
          *) echo "| ${lbl//|/\\|} | failure | $(ci_dur "$d") |" ;;
        esac
      done < "$CI_STEPS"
    else
      echo "_No instrumented command or check steps were recorded for this job. Setup/action step timing remains available in the Actions run view._"
    fi
    echo
    echo "## Cache activity"
    echo
    if [ -s "$CI_CACHES" ]; then
      echo "| Cache | Restore result | Restored size | Matched key | Requested key |"
      echo "|---|---|---:|---|---|"
      while IFS=$'\t' read -r lbl res sz primary matched; do
        echo "| $lbl | $res | ${sz:-—} | ${matched:-—} | ${primary:-—} |"
      done < "$CI_CACHES"
    else
      echo "_No cache restore operation was recorded for this job._"
    fi
    echo
    echo "## Job-specific details"
    echo
    if [ -s "$CI_DETAILS" ]; then
      cat "$CI_DETAILS"
    else
      echo "_This job did not produce additional detail sections._"
    fi
    echo
    if [ -s "$CI_ANNOT" ]; then
      echo "## Error annotations"
      echo
      head -n 40 "$CI_ANNOT"
      echo
    fi
  } > "$report"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$report" >> "$GITHUB_STEP_SUMMARY"; fi
  line="$(awk -F'\t' '{printf "%s=%s ", $1, $2}' "$CI_CACHES")"
  {
    echo "secs=$secs"
    echo "cache=${line:-not-used}"
    echo "errors<<GITRUN_EOF"
    if [ -s "$CI_ANNOT" ]; then head -c 1800 "$CI_ANNOT" | ci_strip; elif [ -s "$CI_TAILS" ]; then head -c 1800 "$CI_TAILS" | ci_strip; fi
    echo
    echo "GITRUN_EOF"
  } >> "${GITHUB_OUTPUT:-/dev/null}"
}
