#!/usr/bin/env bash
# Run every local test suite and print a one-line-per-suite pass/fail summary.
#
# The suites are the same as `tools/regress.sh` (and CI's `check` job), plus
# the AST comparison with pyawk when a checkout sits next to this repository.
# Unlike regress.sh, this script runs every suite even if an earlier one
# fails, then prints a summary. The exit status is 0 only if every suite
# passed (a skipped AST check, when pyawk is absent, is not a failure).
#
# This script never writes to the test trees. Do not add flags such as
# --update-passing, or commands that regenerate corpus files.
#
# usage: tools/check.sh
set -uo pipefail

cd "$(dirname "$0")/.."

# Prefer a rustup toolchain (it has rustfmt and clippy) over a distro cargo.
PATH="$HOME/.cargo/bin:$PATH"
export PATH
# CI denies rustc warnings too; keep the local check aligned with that.
export RUSTFLAGS="${RUSTFLAGS:--D warnings}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

LOGDIR=$(mktemp -d "${TMPDIR:-/tmp}/hawk-check.XXXXXX")
trap 'rm -rf "$LOGDIR"' EXIT

RESULTS=()
FAILED=0

pass() { RESULTS+=("$1|PASS|$2"); }
fail() { RESULTS+=("$1|FAIL|$2"); FAILED=1; }
skip() { RESULTS+=("$1|SKIP|$2"); }

run_logged() {
  local log=$1
  shift
  "$@" >"$log" 2>&1
  return $?
}

echo "hawk check: logs in $LOGDIR"
echo

echo "==> cargo fmt --check"
if run_logged "$LOGDIR/fmt.log" cargo fmt --check; then
  pass "cargo fmt" "clean"
else
  fail "cargo fmt" "see $LOGDIR/fmt.log"
  cat "$LOGDIR/fmt.log"
fi

echo "==> cargo clippy"
if run_logged "$LOGDIR/clippy.log" cargo clippy --all-targets -- -D warnings; then
  pass "cargo clippy" "no warnings"
else
  fail "cargo clippy" "see $LOGDIR/clippy.log"
  tail -40 "$LOGDIR/clippy.log"
fi

echo "==> cargo test"
if run_logged "$LOGDIR/test.log" cargo test --quiet; then
  # Prefer the final "test result: ok." line, which includes the doctest run.
  # Sum passed/failed counts across lib, integration and doctest runs.
  passed=$(awk '/test result:/{n+=$4} END{print n+0}' "$LOGDIR/test.log")
  failed=$(awk '/test result:/{n+=$6} END{print n+0}' "$LOGDIR/test.log")
  if [[ $failed -eq 0 ]]; then
    pass "cargo test" "${passed} passed, 0 failed"
  else
    fail "cargo test" "${passed} passed, ${failed} failed (see $LOGDIR/test.log)"
  fi
else
  summary=$(grep 'test result:' "$LOGDIR/test.log" | tail -1)
  fail "cargo test" "${summary:-failed} (see $LOGDIR/test.log)"
  tail -40 "$LOGDIR/test.log"
fi

echo "==> cargo build --release"
if run_logged "$LOGDIR/build.log" cargo build --quiet --release; then
  pass "release build" "target/release/hawk"
else
  fail "release build" "see $LOGDIR/build.log"
  tail -40 "$LOGDIR/build.log"
fi

echo "==> tools/test_tools.py"
if run_logged "$LOGDIR/test-tools.log" python3 tools/test_tools.py; then
  pass "harness unit tests" "tools/test_tools.py"
else
  fail "harness unit tests" "see $LOGDIR/test-tools.log"
  tail -30 "$LOGDIR/test-tools.log"
fi

echo "==> tools/run_corpus.py"
if run_logged "$LOGDIR/corpus.log" python3 tools/run_corpus.py; then
  corpus_ok=1
else
  corpus_ok=0
  echo "run_corpus.py failed; last lines:"
  tail -40 "$LOGDIR/corpus.log"
fi
eval "$(awk '
  $1=="onetrue" { o_p=$2; o_f=$3; o_s=$4 }
  $1=="gawk"    { g_p=$2; g_f=$3; g_s=$4 }
  $1=="TOTAL"   { t_p=$2; t_f=$3; t_s=$4 }
  END {
    own_p = t_p - o_p - g_p
    own_f = t_f - o_f - g_f
    own_s = t_s - o_s - g_s
    printf "own_p=%d; own_f=%d; own_s=%d; o_p=%d; o_f=%d; o_s=%d; g_p=%d; g_f=%d; g_s=%d\n",
      own_p+0, own_f+0, own_s+0, o_p+0, o_f+0, o_s+0, g_p+0, g_f+0, g_s+0
  }
' "$LOGDIR/corpus.log")"
# hawk's runner treats a new failure of a previously passing case as a
# regression (nonzero exit) even if the "fail" column in the table is the
# count of cases that did not match expected output this run. Report both.
report_corpus() {
  local name=$1 p=$2 f=$3 s=$4
  local detail="${p} passed, ${f} failed, ${s} skipped"
  if [[ $corpus_ok -eq 1 && $f -eq 0 ]]; then
    pass "$name" "$detail"
  else
    fail "$name" "$detail"
  fi
}
report_corpus "golden corpus" "$own_p" "$own_f" "$own_s"
report_corpus "onetrue suite" "$o_p" "$o_f" "$o_s"
report_corpus "gawk suite" "$g_p" "$g_f" "$g_s"

echo "==> tools/regex_check.py"
if run_logged "$LOGDIR/regex.log" python3 tools/regex_check.py; then
  regex_ok=1
else
  regex_ok=0
  tail -30 "$LOGDIR/regex.log"
fi
# "  c table: 13503 agree, 0 mismatch (hard gate)"
c_line=$(awk '/c table:/{print; exit}' "$LOGDIR/regex.log")
u_line=$(awk '/utf8 table:/{print; exit}' "$LOGDIR/regex.log")
c_ok=$(awk '{print $3}' <<<"$c_line")
c_bad=$(awk '{print $5}' <<<"$c_line")
u_ok=$(awk '{print $3}' <<<"$u_line")
u_bad=$(awk '{print $5}' <<<"$u_line")
if [[ $regex_ok -eq 1 && ${c_bad:-1} -eq 0 ]]; then
  pass "regex differential (C)" "${c_ok} lines agree, ${c_bad} mismatch"
else
  fail "regex differential (C)" "${c_ok:-?} lines agree, ${c_bad:-?} mismatch"
fi
if [[ $regex_ok -eq 1 && ${u_bad:-1} -eq 0 ]]; then
  pass "regex differential (UTF-8)" "${u_ok} lines agree, ${u_bad} mismatch"
else
  fail "regex differential (UTF-8)" "${u_ok:-?} lines agree, ${u_bad:-?} mismatch"
fi

echo "==> tools/ast_check.py"
if [[ -e ../pyawk/.git ]]; then
  if run_logged "$LOGDIR/ast.log" python3 tools/ast_check.py; then
    ast_summary=$(grep 'programs:' "$LOGDIR/ast.log" | tail -1)
    pass "ast check vs pyawk" "${ast_summary:-ok}"
  else
    ast_summary=$(grep 'programs:\|MISMATCH' "$LOGDIR/ast.log" | tail -5 | tr '\n' ' ')
    fail "ast check vs pyawk" "${ast_summary:-failed} (see $LOGDIR/ast.log)"
    tail -20 "$LOGDIR/ast.log"
  fi
else
  skip "ast check vs pyawk" "no ../pyawk checkout (same as tools/regress.sh)"
fi

echo
echo "======== check summary ========"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r name status detail <<<"$row"
  printf '%-28s %-4s  %s\n' "$name" "$status" "$detail"
done
if [[ $FAILED -eq 0 ]]; then
  echo "======== all suites passed ========"
else
  echo "======== some suites failed ======== (logs in $LOGDIR)"
  trap - EXIT
fi
exit "$FAILED"
