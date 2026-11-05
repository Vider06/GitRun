#!/usr/bin/env bash
set -euo pipefail

# shellcheck disable=SC1090
source "$RUNNER_TEMP/ci-lib.sh"
fail=0
expect() { # name needle haystack
  if grep -qF -- "$2" <<<"$3"; then printf '✅ %s\n' "$1"; else printf '❌ %s — expected to find: %s\n' "$1" "$2"; head -n 6 <<<"$3"; fail=1; fi
}
ws="$GITHUB_WORKSPACE"

# Target cache keys include manifest/lock hashes before the profile/job suffix.
# GC should group superseded keys for the SAME job, never consume another job's
# separately useful target cache; image fingerprints must remain distinct too.
jq -n '
  def fam:
    sub("-run[0-9]+-attempt[0-9]+$"; "")
    | if contains("-target-img") then
        sub("-man[^-]+-[^-]+-prof"; "-prof")
      else sub("-[^-]+$"; "") end;
  [
    "gitrun-e1-Linux-X64-target-imgdeadbeef-rustc1.99.0-cfg12345678-man11111111-lock11111111-profaaaaaaaa-rust-lint",
    "gitrun-e1-Linux-X64-target-imgdeadbeef-rustc1.99.0-cfg12345678-man22222222-lock22222222-profaaaaaaaa-rust-lint",
    "gitrun-e1-Linux-X64-target-imgdeadbeef-rustc1.99.0-cfg12345678-man22222222-lock22222222-profaaaaaaaa-rust-test",
    "gitrun-e1-Linux-X64-target-imgfeedbeef-rustc1.99.0-cfg12345678-man22222222-lock22222222-profaaaaaaaa-rust-lint"
  ] as $keys
  | if (($keys[0] | fam) == ($keys[1] | fam))
       and (($keys[0] | fam) != ($keys[2] | fam))
       and (($keys[0] | fam) != ($keys[3] | fam))
    then true
    else error("target cache GC family normalization is unsafe")
    end
' >/dev/null
echo "✅ target cache GC family normalization"

out="$(printf '%s\n' '{"reason":"compiler-message","message":{"rendered":"error: x\n","level":"error","message":"unneeded return, 50% sure","code":{"code":"clippy::needless_return"},"spans":[{"file_name":"'"$ws"'/crates/b/src/main.rs","is_primary":true,"line_start":7,"line_end":7,"column_start":5,"column_end":12}]}}' | ci_cargo_json)"
expect "cargo json → annotation with relative path" '::error file=crates/b/src/main.rs,line=7,col=5' "$out"
expect "cargo json → % escaped" '50%25 sure' "$out"
out="$(printf 'Diff in %s/src/lib.rs:12:\n' "$ws" | ci_awk_fmt)"
expect "rustfmt diff → annotation" '::error file=src/lib.rs,line=12,title=rustfmt' "$out"

out="$(printf "thread 'tests::bad' panicked at crates/a/src/lib.rs:10:9:\nboom\n" | ci_awk_tests)"
expect "test panic → annotation" '::error file=crates/a/src/lib.rs,line=10,col=9,title=test failed%3A tests%3A%3Abad::boom' "$out"

out="$(printf 'a.sh:3:5: warning: Quote this. [SC2086]\n' | ci_awk_gcc)"
expect "shellcheck gcc → annotation" '::warning file=a.sh,line=3,col=5,title=SC2086::Quote this.' "$out"

out="$(printf 'Dockerfile:3 DL3025 warning: Use JSON notation\n' | ci_awk_hadolint)"
expect "hadolint → annotation" '::warning file=Dockerfile,line=3,title=DL3025' "$out"

out="$(printf 'Crate:     time\nVersion:   0.1.45\nTitle:     Segfault\nID:        RUSTSEC-2020-0071\nSolution:  Upgrade to >=0.2.23\n\nerror: 1 vulnerability found!\n' | ci_awk_audit)"
expect "cargo audit vulnerability → error on Cargo.lock" '::error file=Cargo.lock,title=RUSTSEC-2020-0071' "$out"
out="$(printf 'Crate:     atty\nVersion:   0.2.14\nWarning:   unmaintained\nTitle:     atty is unmaintained\nID:        RUSTSEC-2024-0375\n' | ci_awk_audit)"
expect "cargo audit unmaintained → warning" '::warning file=Cargo.lock,title=RUSTSEC-2024-0375' "$out"

out="$(printf 'braces  <3.0.3\nSeverity: high\nBad thing in braces\n' | ci_awk_npm app/package.json)"
expect "npm audit high → error" '::error file=app/package.json,title=npm audit high' "$out"
out="$(printf '%s/web/a.js:4\nfoo(\n   ^\n\nSyntaxError: Unexpected token\n' "$ws" | ci_awk_node)"
expect "node --check → annotation" '::error file=web/a.js,line=4,title=node --check' "$out"
out="$(printf 'x.sh: line 4: syntax error near unexpected token `fi\x27\n' | ci_awk_bashn)"
expect "bash -n → annotation" '::error file=x.sh,line=4,title=bash -n' "$out"
out="$(printf 'a.txt:2: trailing whitespace.\n' | ci_awk_diffcheck warning)"
expect "git diff --check → annotation" '::warning file=a.txt,line=2,title=whitespace' "$out"

expect "property escaping" 'a%3Ab%2Cc' "$(ci_escp 'a:b,c')"

# Exercise the shared per-job report with isolated files so the test
# cannot pollute this job's real summary, cache data or error report.
test_dir="$RUNNER_TEMP/gitrun-ci-report-selftest"
rm -rf "$test_dir"; mkdir -p "$test_dir/cache-data"
printf 'fixture\n' > "$test_dir/cache-data/item"
saved_ci_dir="$CI_DIR"; saved_ci_steps="$CI_STEPS"; saved_ci_caches="$CI_CACHES"
saved_ci_checks="$CI_CHECKS"; saved_ci_annot="$CI_ANNOT"; saved_ci_tails="$CI_TAILS"
saved_ci_details="$CI_DETAILS"; saved_summary="$GITHUB_STEP_SUMMARY"
CI_DIR="$test_dir"; CI_LOGS="$test_dir/logs"; CI_STEPS="$test_dir/steps.tsv"
CI_CACHES="$test_dir/caches.tsv"; CI_CHECKS="$test_dir/checks.tsv"
CI_ANNOT="$test_dir/annotations.txt"; CI_TAILS="$test_dir/tails.txt"
CI_DETAILS="$test_dir/details.md"; GITHUB_STEP_SUMMARY="$test_dir/summary.md"
mkdir -p "$CI_LOGS"; touch "$CI_STEPS" "$CI_CACHES" "$CI_CHECKS" "$CI_ANNOT" "$CI_TAILS" "$CI_DETAILS"
date +%s > "$CI_DIR/t0"; date -u +'%Y-%m-%dT%H:%M:%SZ' > "$CI_DIR/started_at"
export GITRUN_JOB_STATUS=success
ci_check error "report self-test success" true
CI_SUPPRESS_ANNOTATIONS=1 ci_check warn "report self-test warning" false > /dev/null
ci_gate error "report self-test gate" true
ci_cache_note "report-cache" "requested-key" "restored-partial-key" "$test_dir/cache-data"
echo "- Runtime-generated detail fixture." | ci_sum
ci_report
expect "standard report has timing section" "## Step results and timings" "$(cat "$GITHUB_STEP_SUMMARY")"
expect "standard report has cache section" "## Cache activity" "$(cat "$GITHUB_STEP_SUMMARY")"
expect "standard report identifies partial cache restore" "partial" "$(cat "$GITHUB_STEP_SUMMARY")"
expect "standard report shows requested and matched cache keys" "restored-partial-key" "$(cat "$GITHUB_STEP_SUMMARY")"
expect "standard report preserves job-specific details" "Runtime-generated detail fixture" "$(cat "$GITHUB_STEP_SUMMARY")"
expect "ci_check and ci_gate append timed rows" "report self-test gate" "$(cat "$CI_STEPS")"
if awk -F '\t' '$1 == "report self-test warning" && $2 == "warn" { found=1 } END { exit !found }' "$CI_STEPS"; then
  echo "✅ ci_check records warning result"
else
  echo "❌ ci_check did not record the warning result"
  fail=1
fi
expect "standard report marks warning step" "| report self-test warning | warning |" "$(cat "$GITHUB_STEP_SUMMARY")"
CI_DIR="$saved_ci_dir"; CI_STEPS="$saved_ci_steps"; CI_CACHES="$saved_ci_caches"
CI_CHECKS="$saved_ci_checks"; CI_ANNOT="$saved_ci_annot"; CI_TAILS="$saved_ci_tails"
CI_DETAILS="$saved_ci_details"; GITHUB_STEP_SUMMARY="$saved_summary"
unset GITRUN_JOB_STATUS
if ci_run "intentional failure (exit code is propagated)" bash -c 'exit 7' > /dev/null; then echo "❌ ci_run swallowed a failure"; fail=1; else echo "✅ ci_run propagates failures"; fi
if CI_LEVEL=warning ci_run "advisory failure" bash -c 'exit 7' > /dev/null; then echo "✅ advisory runs do not fail"; else echo "❌ advisory run failed"; fail=1; fi
printf 'x' > "$RUNNER_TEMP/selftest.bin"
if ci_tool selftest 1 "file://$RUNNER_TEMP/selftest.bin" 0000 bin > /dev/null 2>&1; then echo "❌ checksum mismatch was accepted"; fail=1; else echo "✅ checksum mismatch is rejected"; fi
# The self-test deliberately produced failures: they must not leak into this job's report.
: > "$CI_ANNOT"; : > "$CI_TAILS"; : > "$CI_STEPS"
[ "$fail" -eq 0 ] || { ci_err "CI helper self-test failed: a log parser or helper is broken"; exit 1; }
echo "all helper self-tests passed"
