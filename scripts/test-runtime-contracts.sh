#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-$ROOT/target/debug/gitrun}"
BIN="$(realpath "$BIN")"
test -x "$BIN" || { echo "GitRun binary missing or not executable: $BIN" >&2; exit 1; }

EXPECTED_VERSION="$(tr -d '[:space:]' < "$ROOT/version.txt")"
VERSION_OUTPUT="$("$BIN" --no-cat -V --gitrun)"
if [[ "$VERSION_OUTPUT" != "GitRun $EXPECTED_VERSION" ]]; then
  printf 'Version contract failed: expected %q, got %q\n' "GitRun $EXPECTED_VERSION" "$VERSION_OUTPUT" >&2
  exit 1
fi

TMP="$(mktemp -d)"
WATCHDOG_PID=""
CHILD_PID=""
cleanup() {
  if [[ -n "$CHILD_PID" ]] && kill -0 "$CHILD_PID" 2>/dev/null; then
    kill "$CHILD_PID" 2>/dev/null || true
    wait "$CHILD_PID" 2>/dev/null || true
  fi
  if [[ -n "$WATCHDOG_PID" ]] && kill -0 "$WATCHDOG_PID" 2>/dev/null; then
    kill -TERM "$WATCHDOG_PID" 2>/dev/null || true
    wait "$WATCHDOG_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

STATE_DIR="$TMP/state"
LOG_DIR="$TMP/log"
mkdir -p "$STATE_DIR" "$LOG_DIR"
sed \
  -e "s|^GITRUN_STATE_DIR=.*|GITRUN_STATE_DIR=$STATE_DIR|" \
  -e "s|^GITRUN_LOG_DIR=.*|GITRUN_LOG_DIR=$LOG_DIR|" \
  "$ROOT/config/config.example.env" > "$TMP/gitrun.env"

# Launch the real compiled CLI as a second process. GITRUN_CONFIG_FILE isolates
# the test from host configuration, while the production parser still loads it.
GITRUN_CONFIG_FILE="$TMP/gitrun.env" "$BIN" gsr-watchdog > "$TMP/watchdog.log" 2>&1 &
WATCHDOG_PID=$!

for _ in $(seq 1 50); do
  if grep -q "gitrun-gsr: watching gitrun-autoscaler" "$TMP/watchdog.log"; then
    break
  fi
  if ! kill -0 "$WATCHDOG_PID" 2>/dev/null; then
    cat "$TMP/watchdog.log" >&2
    echo "GSR watchdog exited before reporting readiness" >&2
    exit 1
  fi
  sleep 0.2
done
if ! grep -q "gitrun-gsr: watching gitrun-autoscaler" "$TMP/watchdog.log"; then
  cat "$TMP/watchdog.log" >&2
  echo "GSR watchdog did not become ready" >&2
  exit 1
fi

# The watchdog only reports an unexpected exit after it has seen a live PID.
sleep 60 &
CHILD_PID=$!
printf '%s\n' "$CHILD_PID" > "$STATE_DIR/gitrun-autoscaler.pid"
sleep 11
kill -0 "$CHILD_PID" 2>/dev/null || {
  cat "$TMP/watchdog.log" >&2
  echo "test child exited before watchdog observed it alive" >&2
  exit 1
}
kill "$CHILD_PID"
wait "$CHILD_PID" 2>/dev/null || true
CHILD_PID=""

for _ in $(seq 1 30); do
  if grep -R -q "exited unexpectedly" "$STATE_DIR"; then
    echo "PASS: runtime CLI started a separate GSR watchdog and recorded the unexpected child exit"
    exit 0
  fi
  if ! kill -0 "$WATCHDOG_PID" 2>/dev/null; then
    cat "$TMP/watchdog.log" >&2
    echo "GSR watchdog exited before recording the crash event" >&2
    exit 1
  fi
  sleep 0.5
done

find "$STATE_DIR" -type f -maxdepth 3 -print -exec cat {} \; >&2 || true
cat "$TMP/watchdog.log" >&2
echo "GSR watchdog did not persist the expected crash event" >&2
exit 1
