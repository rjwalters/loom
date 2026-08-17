#!/usr/bin/env bash
# test-run-ci-suites-daemon-guard.sh — the live-daemon guard in
# run-ci-suites.sh (issue #6386).
#
# #6386: an Auditor ran `bash defaults/scripts/tests/run-ci-suites.sh` from a
# fleet host's LIVE checkout. The daemon-lifecycle suites execute the real
# loom-daemon-{start,stop,update,quiesce}.sh, one of their cases resolved the
# live `.loom/.daemon.pid`, and the fleet's authoritative dispatcher was
# SIGTERM'd for 11 hours. The per-suite sandboxes are necessary but not
# sufficient — one forgotten pin in one case is enough — so run-ci-suites.sh
# now refuses to run those suites at all when a daemon pid file exists on the
# host.
#
# Driven entirely through `run-ci-suites.sh --plan`, which prints the RUN/SKIP
# decision for every wired suite and exits WITHOUT executing any of them. That
# keeps this suite hermetic (nothing is started, killed, or written outside
# $WORKDIR) and fast.
#
# Every case pins the candidate pid-file list via the guard's TEST-ONLY seam
# LOOM_CI_DAEMON_PIDFILE_CANDIDATES (`none` = no candidates at all), so no
# assertion depends on whether THIS host happens to be running a real daemon —
# which is exactly the condition under test, and would otherwise make "the
# guard is not always-on" unfalsifiable on a fleet host and "the guard fires"
# unfalsifiable on a CI runner.
#
# Usage:
#   ./defaults/scripts/tests/test-run-ci-suites-daemon-guard.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNNER="$SCRIPT_DIR/run-ci-suites.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} $1"
}
fail() {
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} $1"
    [[ -n "${2:-}" ]] && echo "$2" | sed 's/^/    /'
}
check() {
    local rc="$1" msg="$2" detail="${3:-}"
    if [[ "$rc" -eq 0 ]]; then pass "$msg"; else fail "$msg" "$detail"; fi
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# The five host-mutating suites the guard owns. Kept as an explicit literal
# list (not scraped from the runner) so a silent narrowing of the guard's own
# LIVE_DAEMON_GUARDED_SUITES is a test failure, not an invisible regression.
GUARDED_SUITES=(
    test-loom-daemon-start.sh
    test-loom-daemon-stop.sh
    test-loom-daemon-update.sh
    test-loom-daemon-quiesce.sh
    test-loom-daemon-watchdog.sh
)

plan_line() { # <plan output> <suite>
    printf '%s\n' "$1" | awk -v s="$2" '$2 == s { print $1; exit }'
}

# Every --plan invocation below pins the candidate list, so nothing about the
# host running this suite can influence the outcome.
run_plan() { # <pidfile-candidates|none> [stderr-path]
    LOOM_CI_DAEMON_PIDFILE_CANDIDATES="$1" bash "$RUNNER" --plan 2>"${2:-/dev/null}"
}

# ---------- 1. no pid file anywhere -> every daemon suite is planned to RUN ----
ABSENT_PLAN="$( run_plan "$WORKDIR/absent-a.pid:$WORKDIR/absent-b.pid" )"
absent_rc=$?
check "$absent_rc" "no daemon pid file: --plan exits 0"

missing=""
for suite in "${GUARDED_SUITES[@]}"; do
    [[ "$(plan_line "$ABSENT_PLAN" "$suite")" == "RUN" ]] || missing="$missing $suite"
done
check "$([[ -z "$missing" ]] && echo 0 || echo 1)" \
    "no daemon pid file: all five daemon-lifecycle suites are planned to RUN (guard is not always-on)" \
    "not planned RUN:$missing"

# A non-daemon suite is a control: it must RUN in both directions below.
check "$([[ "$(plan_line "$ABSENT_PLAN" "test-live-state-sandbox.sh")" == "RUN" ]] && echo 0 || echo 1)" \
    "no daemon pid file: an unrelated suite is planned to RUN (control)"

# ---------- 2. a live-looking pid file -> the daemon suites are SKIPPED --------
# "Live-looking" without ever naming a real daemon: the pid recorded is this
# test process's own, which is unambiguously alive and unambiguously not a
# daemon. Nothing in this suite ever signals it.
LIVE_PID_FILE="$WORKDIR/live/.loom/.daemon.pid"
mkdir -p "$(dirname "$LIVE_PID_FILE")"
echo "$$" > "$LIVE_PID_FILE"

LIVE_PLAN_ERR="$WORKDIR/live-plan.err"
LIVE_PLAN="$( run_plan "$LIVE_PID_FILE" "$LIVE_PLAN_ERR" )"
live_rc=$?
check "$live_rc" "live pid file: --plan still exits 0 (a skip is not a failure)"

not_skipped=""
for suite in "${GUARDED_SUITES[@]}"; do
    [[ "$(plan_line "$LIVE_PLAN" "$suite")" == "SKIP" ]] || not_skipped="$not_skipped $suite"
done
check "$([[ -z "$not_skipped" ]] && echo 0 || echo 1)" \
    "live pid file: all five daemon-lifecycle suites are SKIPPED (#6386 hazard is unreachable)" \
    "not skipped:$not_skipped"

check "$([[ "$(plan_line "$LIVE_PLAN" "test-live-state-sandbox.sh")" == "RUN" ]] && echo 0 || echo 1)" \
    "live pid file: unrelated suites still RUN (the guard is scoped, not a blanket refusal)"

# The skip must be LOUD — an invisible skip reads as "the suites passed".
guard_err="$(cat "$LIVE_PLAN_ERR" 2>/dev/null)"
check "$([[ "$guard_err" == *"LIVE DAEMON DETECTED"* ]] && echo 0 || echo 1)" \
    "live pid file: the guard announces itself loudly on stderr" "$guard_err"
check "$([[ "$guard_err" == *"$LIVE_PID_FILE"* ]] && echo 0 || echo 1)" \
    "live pid file: the guard names the exact pid file it found" "$guard_err"
check "$([[ "$guard_err" == *"LOOM_CI_ALLOW_DAEMON_SUITES"* ]] && echo 0 || echo 1)" \
    "live pid file: the guard names the override that re-enables the suites" "$guard_err"

# ---------- 3. a STALE pid file also trips the guard --------------------------
# The lifecycle suites `rm -f` whichever pid file they resolve, so even a pid
# file naming a dead process is host state they must not silently delete (and
# it is the operator's evidence of how the daemon last exited).
STALE_PID_FILE="$WORKDIR/stale/.loom/.daemon.pid"
mkdir -p "$(dirname "$STALE_PID_FILE")"
echo "2147483646" > "$STALE_PID_FILE"   # far above any live pid on a real host
STALE_PLAN="$( run_plan "$STALE_PID_FILE" )"
check "$([[ "$(plan_line "$STALE_PLAN" "test-loom-daemon-stop.sh")" == "SKIP" ]] && echo 0 || echo 1)" \
    "stale pid file: still skipped (the suites rm -f whatever they resolve)"

# ---------- 4. LOOM_CI_ALLOW_DAEMON_SUITES=1 is an explicit override ----------
OVERRIDE_ERR="$WORKDIR/override.err"
OVERRIDE_PLAN="$( LOOM_CI_ALLOW_DAEMON_SUITES=1 run_plan "$LIVE_PID_FILE" "$OVERRIDE_ERR" )"
check "$([[ "$(plan_line "$OVERRIDE_PLAN" "test-loom-daemon-stop.sh")" == "RUN" ]] && echo 0 || echo 1)" \
    "LOOM_CI_ALLOW_DAEMON_SUITES=1: the daemon suites are planned to RUN again"
check "$([[ "$(cat "$OVERRIDE_ERR")" == *"LOOM_CI_ALLOW_DAEMON_SUITES is set"* ]] && echo 0 || echo 1)" \
    "LOOM_CI_ALLOW_DAEMON_SUITES=1: the override is still announced (never silent)" \
    "$(cat "$OVERRIDE_ERR")"

# ---------- 5. --plan executes nothing ----------------------------------------
# Proven by a canary: --plan must not create the log file a real run writes for
# the very first suite it would execute.
CANARY_LOG="/tmp/ci-suite-test-live-state-sandbox.sh.log"
canary_mtime() { # portable: GNU stat first, then BSD/macOS; "absent" when missing
    [[ -f "$CANARY_LOG" ]] || { echo absent; return 0; }
    stat -c %Y "$CANARY_LOG" 2>/dev/null || stat -f %m "$CANARY_LOG" 2>/dev/null || echo "?"
}
canary_before="$(canary_mtime)"
run_plan "$WORKDIR/absent-c.pid" >/dev/null
canary_after="$(canary_mtime)"
check "$([[ "$canary_before" == "$canary_after" ]] && echo 0 || echo 1)" \
    "--plan runs no suite at all (per-suite log untouched)"

# ---------- 6. an unknown option is rejected ----------------------------------
bad_out="$(bash "$RUNNER" --nope 2>&1)"
bad_rc=$?
check "$([[ "$bad_rc" -eq 1 && "$bad_out" == *"unknown option"* ]] && echo 0 || echo 1)" \
    "an unrecognized option is rejected before anything runs (rc=$bad_rc)" "$bad_out"

# ---------- 7. `none` means no candidates at all ------------------------------
# The seam's empty setting has to be distinguishable from "seam not set", or a
# case that means "this host has nothing" would silently fall back to the
# derived tiers and assert against the REAL host — vacuous on CI, wrong on a
# fleet host.
NONE_PLAN="$( run_plan none )"
check "$([[ "$(plan_line "$NONE_PLAN" "test-loom-daemon-stop.sh")" == "RUN" ]] && echo 0 || echo 1)" \
    "candidates=none: nothing is detected, so the daemon suites RUN (even on a host with a live daemon)"

echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
