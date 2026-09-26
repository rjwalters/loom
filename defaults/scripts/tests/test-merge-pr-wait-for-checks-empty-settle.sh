#!/usr/bin/env bash
# test-merge-pr-wait-for-checks-empty-settle.sh - Unit tests for the
# empty-output false-settle guard in merge-pr.sh's
# _wait_for_checks_then_sync_merge() (#6169).
#
# Bug (#6169): `gh pr checks` (and the check-runs REST endpoint this function
# actually polls via forge_get_check_runs) can return a completely empty
# rollup (zero rows / total_count:0) during a transient forge failure (e.g.
# an intermittent TLS handshake error) -- indistinguishable, on its own, from
# "this repo genuinely has no CI checks configured for this commit". Before
# this fix, _wait_for_checks_then_sync_merge trusted a zero-row read on its
# VERY FIRST poll as "nothing failing, nothing pending -> CLEAN" and returned
# 0 immediately, letting merge-pr.sh proceed straight to a synchronous merge
# without ever having observed real check-run data. Reported live: a Judge
# poller on kicad-tools PR #4792 (2026-08-13) declared CI "settled" 6 minutes
# into a ~40-minute board-test run this exact way.
#
# #9091 (the other half of the same branch): "or the bounded wait elapsed" was
# catastrophic on the case that guard meets most often -- a repo with NO CI
# configured for the changed paths returns zero rows on every poll forever, so
# every `--auto` merge there burned the entire LOOM_AUTO_MERGE_TIMEOUT (600s)
# before merging, and the calling agent's own process cap killed it first
# (2AMLogic/2am#1267: "Proceeding with squash merge..." then no merge, no
# failure, no label change). The zero-row wait is now bounded to
# LOOM_ZERO_CHECKS_SETTLE_POLLS polls WHEN the base branch requires no
# status-check contexts -- still never one read (#6169 holds), but seconds
# instead of ten minutes. Required contexts present, or a lookup that errors,
# keep the full wait. Scenarios (e), (f) and (g) below cover that split.
#
# Fix: track whether a nonzero total_count has EVER been observed
# (observed_checks). A zero-row read is only trusted once observed_checks is
# true (real data has been seen at least once) OR the bounded
# LOOM_AUTO_MERGE_TIMEOUT wait has fully elapsed (at which point continuing
# to wait cannot help either) -- matching the "at least one confirming read"
# discipline the sibling UNSTABLE branch (_UNSTABLE_OBSERVED_PENDING) already
# used.
#
# Strategy (mirrors test-merge-pr-merge-ordering-guard.sh): extract
# _wait_for_checks_then_sync_merge from merge-pr.sh and source it, stub every
# forge_* helper it calls plus `sleep`/`date` (both stubbed so the test runs
# deterministically and instantly -- no real wall-clock waiting), then assert
# on the function's return value, how many times it polled forge_get_check_runs,
# and the info/warning narration it emits.
#
# Usage:
#   ./.loom/scripts/tests/test-merge-pr-wait-for-checks-empty-settle.sh

# SC2034: several globals (PR_JSON, PR_NUMBER, REPO_NWO, GH,
# LOOM_AUTO_MERGE_TIMEOUT, LOOM_AUTO_MERGE_POLL_INTERVAL) are read only by the
# function extracted+sourced from merge-pr.sh, which shellcheck cannot see.
# shellcheck disable=SC2034

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$TEST_DIR/.." && pwd)"
MERGE_PR_SRC="$HELPERS_DIR/merge-pr.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if grep -qF -- "$needle" <<<"$haystack"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

# --- Minimal logging shims the extracted function calls ---
# `error` must exit non-zero to faithfully model the real script's hard
# block; every scenario below runs the function inside a subshell (via
# run_wait) so this exit only tears down that subshell, not the test.
INFO_LOG=""
WARN_LOG=""
info()    { INFO_LOG+="$*"$'\n'; }
warning() { WARN_LOG+="$*"$'\n'; }
error()   { echo "ERROR: $*" >&2; exit 1; }

# --- Extract the function under test from merge-pr.sh and source it ---
FUNCS_FILE="$(mktemp)"
STATE_DIR="$(mktemp -d)"
trap 'rm -f "$FUNCS_FILE" 2>/dev/null || true; rm -rf "$STATE_DIR" 2>/dev/null || true' EXIT
awk '
  /^_wait_for_checks_then_sync_merge\(\) \{/ { capture=1 }
  /^# Handle auto-merge mode/                { capture=0 }
  capture { print }
' "$MERGE_PR_SRC" > "$FUNCS_FILE"

if ! grep -q '_wait_for_checks_then_sync_merge()' "$FUNCS_FILE"; then
    echo -e "${RED}FATAL${NC}: could not extract _wait_for_checks_then_sync_merge from $MERGE_PR_SRC" >&2
    exit 2
fi
# shellcheck disable=SC1090
source "$FUNCS_FILE"

# --- Stub `sleep`, `date`, and `forge_get_check_runs` so every scenario runs
# instantly and deterministically -- no real wall-clock waiting, no flakiness
# from system load affecting how many loop iterations fit in N real seconds.
#
# The function under test invokes both `date +%s` and `forge_get_check_runs`
# through `$(...)` command substitutions, which fork a SUBSHELL each time --
# a plain shell-variable counter (tried first) resets to 0 in every subshell
# and never persists across calls, producing an infinite loop (the deadline
# compared against "1" forever). State files survive subshell forks, so both
# counters and the canned-response queue live on disk instead.
DATE_COUNTER_FILE="$STATE_DIR/date-counter"
FGCR_CALLS_FILE="$STATE_DIR/fgcr-calls"
FGCR_RESPONSES_FILE="$STATE_DIR/fgcr-responses"   # one canned JSON response per line
SLEPT_FILE="$STATE_DIR/slept-seconds"             # sum of every stubbed sleep's argument

# Never actually sleeps, but records what it was ASKED to sleep. That sum is the
# real-world latency the function would have cost, which is the #9091 assertion
# (a no-CI repo must merge in seconds, not the 600s ceiling) -- and it cannot be
# measured by counting polls alone, since the zero-row path uses a shorter
# spacing than the pending path.
sleep() { echo "$(($(cat "$SLEPT_FILE") + ${1:-0}))" > "$SLEPT_FILE"; }
slept_seconds() { cat "$SLEPT_FILE"; }
date() {
    if [[ "${1:-}" == "+%s" ]]; then
        local n
        n=$(($(cat "$DATE_COUNTER_FILE") + 1))
        echo "$n" > "$DATE_COUNTER_FILE"
        echo "$n"
        return 0
    fi
    command date "$@"
}

# --- Stub forge_get_pr_nocache: never "merged concurrently" in these tests ---
forge_get_pr_nocache() { echo '{"merged": false}'; }

# --- Stub forge_get_required_status_check_contexts ---
# The zero-row branch reads it too as of #9091 (it is the discriminator between
# "bounded settle" and "wait out the whole deadline"), so it is now scenario
# state: $REQUIRED_CONTEXTS is its stdout and $REQUIRED_RC its exit code.
# Defaults to the no-required-contexts, lookup-succeeded case.
REQUIRED_CONTEXTS=""
REQUIRED_RC=0
forge_get_required_status_check_contexts() {
    [[ -n "$REQUIRED_CONTEXTS" ]] && printf '%s\n' "$REQUIRED_CONTEXTS"
    return "$REQUIRED_RC"
}

forge_get_check_runs() {
    local calls
    calls=$(($(cat "$FGCR_CALLS_FILE") + 1))
    echo "$calls" > "$FGCR_CALLS_FILE"
    local total_lines
    total_lines=$(wc -l < "$FGCR_RESPONSES_FILE" | tr -d ' ')
    local idx=$calls
    [[ "$idx" -gt "$total_lines" ]] && idx="$total_lines"   # repeat the last canned response once exhausted
    sed -n "${idx}p" "$FGCR_RESPONSES_FILE"
}

fgcr_call_count() { cat "$FGCR_CALLS_FILE"; }

reset_test_state() {
    INFO_LOG=""
    WARN_LOG=""
    echo 0 > "$DATE_COUNTER_FILE"
    echo 0 > "$FGCR_CALLS_FILE"
    echo 0 > "$SLEPT_FILE"
    : > "$FGCR_RESPONSES_FILE"
    PR_JSON='{"head":{"sha":"deadbeef"},"base":{"ref":"main"}}'
    PR_NUMBER=42
    REPO_NWO="owner/repo"
    GH="gh"
    REQUIRED_CONTEXTS=""
    REQUIRED_RC=0
    # #9091's knobs self-default inside the function (with `:=`, so the call
    # leaves them SET in this shell). Unset them here so every scenario starts
    # from the production defaults rather than inheriting the previous one's.
    unset LOOM_ZERO_CHECKS_SETTLE_POLLS LOOM_ZERO_CHECKS_SETTLE_INTERVAL
}

# Appends one canned JSON response line to the forge_get_check_runs queue.
queue_fgcr_response() { echo "$1" >> "$FGCR_RESPONSES_FILE"; }

EMPTY_ROLLUP='{"total_count":0,"check_runs":[]}'
ONE_SUCCESS_ROLLUP='{"total_count":1,"check_runs":[{"name":"build","status":"completed","conclusion":"success"}]}'
ONE_PENDING_ROLLUP='{"total_count":1,"check_runs":[{"name":"build","status":"in_progress","conclusion":null}]}'

echo "Testing _wait_for_checks_then_sync_merge empty-output false-settle guard (#6169)..."

# (a) THE bug, reproduced: the very first poll returns a zero-row rollup
# (the exact shape a transient forge failure produces). Before the fix this
# returned 0 (settled) on that single empty read. After the fix it must NOT
# trust the empty read alone -- it must poll again, and only settle once a
# real (nonzero) rollup confirms nothing is pending.
reset_test_state
LOOM_AUTO_MERGE_TIMEOUT=100
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$EMPTY_ROLLUP"
queue_fgcr_response "$ONE_SUCCESS_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
assert_eq "0" "$rc" "(a) Function still returns 0 once real data confirms settlement"
assert_eq "true" "$([[ $calls -ge 2 ]] && echo true || echo false)" \
  "(a) forge_get_check_runs was polled MORE THAN ONCE (call count=$calls) -- did not trust the first empty read"

# (b) The most literal false-settle case: EVERY poll returns a zero-row
# rollup, AND the base branch HAS required status-check contexts -- so a
# context that has not registered yet is a gate this merge must not jump
# (#6169's danger, preserved verbatim by #9091). The function must still
# terminate (bounded by LOOM_AUTO_MERGE_TIMEOUT, simulated here via the
# stubbed date counter), but it must NOT settle on the first read -- it must
# poll more than once before giving up, and the fallback narration must say so
# explicitly.
reset_test_state
REQUIRED_CONTEXTS="Required Gate"
LOOM_AUTO_MERGE_TIMEOUT=3
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$EMPTY_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
assert_eq "0" "$rc" "(b) Function eventually returns 0 (bounded wait exhausted, not an infinite loop)"
assert_eq "true" "$([[ $calls -ge 2 ]] && echo true || echo false)" \
  "(b) forge_get_check_runs was polled MORE THAN ONCE before giving up (call count=$calls)"
assert_contains "$WARN_LOG" "remained empty" \
  "(b) Warns explicitly that the rollup remained empty for the whole bounded wait, rather than silently declaring settled"

# (c) Regression guard: the common healthy case is unaffected. A rollup that
# is non-empty (real data) on the very first poll, with nothing pending and
# nothing failing, settles immediately -- no unnecessary extra polling for
# the normal case.
reset_test_state
LOOM_AUTO_MERGE_TIMEOUT=100
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$ONE_SUCCESS_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
assert_eq "0" "$rc" "(c) Function returns 0 for a genuinely-settled, nonempty rollup"
assert_eq "1" "$calls" "(c) Only ONE poll needed for the common healthy case (no unnecessary retries)"

# (d) A still-pending check on the first poll is unaffected by the guard --
# it takes the existing pending-wait path, then settles once the check
# resolves.
reset_test_state
LOOM_AUTO_MERGE_TIMEOUT=100
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$ONE_PENDING_ROLLUP"
queue_fgcr_response "$ONE_SUCCESS_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
assert_eq "0" "$rc" "(d) Function returns 0 once the pending check resolves"
assert_eq "2" "$calls" "(d) Exactly two polls: one pending, one resolved"

echo ""
echo "Testing #9091's bounded zero-row settle (no required contexts)..."

# (e) THE #9091 bug, with production defaults: a repo with no CI configured for
# this commit (zero rows on every poll) whose base branch requires no status
# checks. Before the fix this polled until LOOM_AUTO_MERGE_TIMEOUT (600s)
# elapsed -- long enough that the caller's own process cap killed it mid-wait,
# which is how 2AMLogic/2am#1267 got "Proceeding with squash merge..." and then
# no merge at all. After the fix it settles after LOOM_ZERO_CHECKS_SETTLE_POLLS
# polls, and the accumulated wait must be well under 30s (the issue's stated
# regression bar).
reset_test_state
LOOM_AUTO_MERGE_TIMEOUT=600
LOOM_AUTO_MERGE_POLL_INTERVAL=30
queue_fgcr_response "$EMPTY_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
slept="$(slept_seconds)"
assert_eq "0" "$rc" "(e) Function returns 0 (settled) on a zero-check repo with no required contexts"
assert_eq "3" "$calls" "(e) Exactly LOOM_ZERO_CHECKS_SETTLE_POLLS polls at the production default (3) -- not one (that was #6169), not the whole deadline"
assert_eq "true" "$([[ $slept -lt 30 ]] && echo true || echo false)" \
  "(e) Total wait was under 30s (simulated ${slept}s), not the 600s LOOM_AUTO_MERGE_TIMEOUT ceiling"
assert_contains "$INFO_LOG" "requires no status-check contexts" \
  "(e) Narrates WHY the zero-row read was trusted early (no required contexts on the base branch)"
assert_eq "" "$WARN_LOG" "(e) No timeout warning -- the deadline was never reached"

# (f) Fail-closed: the required-context lookup itself errors. Unknown protection
# is not evidence of absent protection (the same disposition the failing-check
# branch above takes), so the bounded settle must NOT apply -- the full #6169
# wait stands.
reset_test_state
REQUIRED_RC=1
LOOM_AUTO_MERGE_TIMEOUT=6
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$EMPTY_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
calls="$(fgcr_call_count)"
assert_eq "0" "$rc" "(f) Function still terminates when the required-context lookup fails"
assert_eq "true" "$([[ $calls -gt 2 ]] && echo true || echo false)" \
  "(f) A failed lookup keeps the FULL bounded wait (call count=$calls > 2), not the shortened settle"
assert_contains "$WARN_LOG" "remained empty" \
  "(f) Fail-closed path still ends in the whole-wait-elapsed warning"

# (g) The required-context lookup is made ONCE per call, not once per poll: a
# zero-row repo polled N times must not spend N branch-protection API reads.
reset_test_state
REQUIRED_CONTEXTS="Required Gate"
# Counts on DISK, not in a variable: the function calls this helper inside a
# `$(...)` command substitution, so a shell-variable counter would be
# incremented in a subshell and lost every time (same reason as the stubs above).
echo 0 > "$STATE_DIR/required-lookups"
forge_get_required_status_check_contexts() {
    echo "$(($(cat "$STATE_DIR/required-lookups") + 1))" > "$STATE_DIR/required-lookups"
    printf '%s\n' "$REQUIRED_CONTEXTS"
}
LOOM_AUTO_MERGE_TIMEOUT=6
LOOM_AUTO_MERGE_POLL_INTERVAL=1
queue_fgcr_response "$EMPTY_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
assert_eq "0" "$rc" "(g) Function returns 0 after the full wait with required contexts present"
assert_eq "1" "$(cat "$STATE_DIR/required-lookups")" \
  "(g) Required-context set resolved exactly ONCE and cached across every zero-row poll"
# Restore the scenario-state stub for anything added after this point.
forge_get_required_status_check_contexts() {
    [[ -n "$REQUIRED_CONTEXTS" ]] && printf '%s\n' "$REQUIRED_CONTEXTS"
    return "$REQUIRED_RC"
}

# (h) The knob cannot be turned back into the #6169 bug: settling on a SINGLE
# empty read is what that issue was, so LOOM_ZERO_CHECKS_SETTLE_POLLS=1 (and
# any non-numeric value) is floored at 2 rather than honoured.
for bad_polls in 1 0 abc; do
    reset_test_state
    LOOM_ZERO_CHECKS_SETTLE_POLLS="$bad_polls"
    LOOM_AUTO_MERGE_TIMEOUT=600
    LOOM_AUTO_MERGE_POLL_INTERVAL=30
    queue_fgcr_response "$EMPTY_ROLLUP"
    _wait_for_checks_then_sync_merge
    rc=$?
    calls="$(fgcr_call_count)"
    assert_eq "0" "$rc" "(h) Function returns 0 with LOOM_ZERO_CHECKS_SETTLE_POLLS='$bad_polls'"
    assert_eq "2" "$calls" "(h) LOOM_ZERO_CHECKS_SETTLE_POLLS='$bad_polls' is floored at 2 polls, never 1 (#6169 stays closed)"
done

# (i) A non-numeric interval must not reach `sleep` as a bad argument: it falls
# back to LOOM_AUTO_MERGE_POLL_INTERVAL (the conservative, longer spacing).
reset_test_state
LOOM_ZERO_CHECKS_SETTLE_INTERVAL="not-a-number"
LOOM_AUTO_MERGE_TIMEOUT=600
LOOM_AUTO_MERGE_POLL_INTERVAL=30
queue_fgcr_response "$EMPTY_ROLLUP"
_wait_for_checks_then_sync_merge
rc=$?
assert_eq "0" "$rc" "(i) Function returns 0 with a non-numeric LOOM_ZERO_CHECKS_SETTLE_INTERVAL"
assert_eq "60" "$(slept_seconds)" \
  "(i) Non-numeric interval fell back to LOOM_AUTO_MERGE_POLL_INTERVAL (2 x 30s), not a broken sleep"

echo ""
echo "=== Test Summary ==="
echo "Total:  $TESTS_RUN"
echo -e "Passed: ${GREEN}$TESTS_PASSED${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
else
    echo -e "Failed: $TESTS_FAILED"
    exit 0
fi
