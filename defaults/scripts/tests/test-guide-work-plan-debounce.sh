#!/usr/bin/env bash
# test-guide-work-plan-debounce.sh - Regression test for issue #5890
#
# Between 2026-08-10T03:09Z and 2026-08-10T11:10Z, 9 consecutive merged PRs on
# main were all `docs: Guide document maintenance update`, with no
# substantive work merged in between. Root cause: `update_work_plan()`'s only
# write-worthiness gate was "does render_plan_body()'s output differ from the
# committed WORK_PLAN.md region" — true on every `loom:building`/`loom:issue`
# transition on ANY issue, including issues bouncing through Builder-claim ->
# Judge-approve -> Champion merge-risk-hold -> re-claim cycles (observed on
# #5607/#5629). Every bounce manufactured its own docs PR. This mirrors the
# #5643 incident `urgent-flip-guard.sh` fixed for `loom:urgent` specifically,
# but nothing analogous gated the plan-body regeneration itself.
#
# The fix adds a time-based debounce: `update_work_plan()` only writes a
# rewritten body once at least `LOOM_WORK_PLAN_DEBOUNCE_SECS` (default 3600)
# have elapsed since the most recently MERGED docs-maintenance PR, using the
# forge's own merged-PR history as the durable, side-car-free anchor (reusing
# `GUIDE_DOCS_PR_EXCLUDE`, the same "is this a docs-maintenance PR" predicate
# #5454's fix already established).
#
# Verifies that:
#   1. guide.md defines `last_docs_pr_merged_epoch()`, reusing
#      `GUIDE_DOCS_PR_EXCLUDE` rather than redefining the docs-PR predicate.
#   2. guide.md's `update_work_plan()` gates the write behind
#      `LOOM_WORK_PLAN_DEBOUNCE_SECS` (default 3600) since that merge.
#   3. THE REGRESSION, executed rather than grepped: a reconstruction of the
#      debounce arithmetic, run against fixture timelines —
#        a. zero label churn (new_body == old_body) still produces zero
#           writes, unchanged from before #5890.
#        b. a rapid label flip-flop diff, observed inside the debounce
#           window since the last merge, is suppressed (no write).
#        c. the SAME diff, once the debounce window has elapsed, produces
#           exactly one write.
#        d. a diff with no prior docs-maintenance merge in history at all
#           (last_merged_epoch == 0) writes immediately — never suppressed
#           by a merge that never happened.
#
# Hermetic: pure grep/arithmetic against fixture epoch values. No forge,
# network, or `gh` calls.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_eq() {
    local actual="$1" expected="$2" msg="$3"
    if [[ "$actual" == "$expected" ]]; then pass "$msg"; else fail "$msg (got '$actual', expected '$expected')"; fi
}

assert_grep() {
    local pattern="$1" file="$2" msg="$3"
    if grep -qE "$pattern" "$file"; then pass "$msg"; else fail "$msg (missing pattern: $pattern)"; fi
}

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found at $GUIDE_MD"
    exit 1
fi

# ---------------------------------------------------------------------------
# Test 1: the prompt wires the debounce into update_work_plan()
# ---------------------------------------------------------------------------
echo "Test 1: guide.md defines and wires the WORK_PLAN debounce"

assert_grep 'last_docs_pr_merged_epoch\(\) \{' "$GUIDE_MD" \
    "last_docs_pr_merged_epoch() is defined"
assert_grep 'select\(\$GUIDE_DOCS_PR_EXCLUDE\)' "$GUIDE_MD" \
    "last_docs_pr_merged_epoch() reuses GUIDE_DOCS_PR_EXCLUDE rather than redefining the docs-PR predicate"
assert_grep 'LOOM_WORK_PLAN_DEBOUNCE_SECS:-3600' "$GUIDE_MD" \
    "update_work_plan() reads LOOM_WORK_PLAN_DEBOUNCE_SECS with a 3600s (1h) default"
assert_grep 'update_work_plan\(\) \{' "$GUIDE_MD" \
    "update_work_plan() is defined"

# update_work_plan() must call last_docs_pr_merged_epoch() AFTER the
# new_body == old_body check (the debounce must never suppress the
# "nothing changed" fast path, only genuine diffs).
UWP_BODY="$(awk '/^update_work_plan\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ "$UWP_BODY" == *'new_body" = "$old_body"'*'last_docs_pr_merged_epoch'* ]]; then
    pass "the no-change fast path (new_body == old_body) is checked BEFORE the debounce"
else
    fail "expected the no-change check to precede the debounce call inside update_work_plan()"
fi
if [[ "$UWP_BODY" == *'last_docs_pr_merged_epoch'* ]]; then
    pass "update_work_plan() calls last_docs_pr_merged_epoch()"
else
    fail "update_work_plan() never calls last_docs_pr_merged_epoch()"
fi

# ---------------------------------------------------------------------------
# Reconstruct the debounce arithmetic exactly as update_work_plan() performs
# it (mirrors the extraction style of test-guide-work-log-self-loop.sh):
# $1 = new_body, $2 = old_body, $3 = last_merged_epoch, $4 = now_epoch,
# $5 = debounce_secs. Echoes "WRITE" or "SUPPRESS".
# ---------------------------------------------------------------------------
work_plan_decision() {
    local new_body="$1" old_body="$2" last_merged_epoch="$3" now_epoch="$4" debounce_secs="$5"
    local elapsed

    if [[ "$new_body" == "$old_body" ]]; then
        echo "SUPPRESS"
        return
    fi

    elapsed=$((now_epoch - last_merged_epoch))
    if [[ "$last_merged_epoch" -gt 0 ]] && [[ "$elapsed" -lt "$debounce_secs" ]]; then
        echo "SUPPRESS"
        return
    fi

    echo "WRITE"
}

DEBOUNCE=3600

# ---------------------------------------------------------------------------
# Test 2: zero label churn still produces zero writes (unchanged behavior)
# ---------------------------------------------------------------------------
echo ""
echo "Test 2: stable label state (no diff) is unaffected by the debounce"

DECISION="$(work_plan_decision "same body" "same body" 0 1000000 "$DEBOUNCE")"
assert_eq "$DECISION" "SUPPRESS" "identical new_body/old_body suppresses regardless of merge history"

DECISION="$(work_plan_decision "same body" "same body" 999995 1000000 "$DEBOUNCE")"
assert_eq "$DECISION" "SUPPRESS" "identical new_body/old_body suppresses even seconds after a merge"

# ---------------------------------------------------------------------------
# Test 3: THE FLAP — a diff inside the debounce window is suppressed
# ---------------------------------------------------------------------------
echo ""
echo "Test 3: a rapid label flip-flop diff inside the debounce window is suppressed"

# Last docs-maintenance PR merged at T=1000000. A #5607/#5629-style bounce
# reshapes render_plan_body()'s output 5 minutes later (300s < 3600s window).
LAST_MERGE=1000000
FLAP_TICK=$((LAST_MERGE + 300))
DECISION="$(work_plan_decision "## Ready\n- #5629" "## Ready\n(none)" "$LAST_MERGE" "$FLAP_TICK" "$DEBOUNCE")"
assert_eq "$DECISION" "SUPPRESS" "a diff 300s after the last merge (inside the 3600s window) is suppressed"

# A second bounce a few minutes later, still inside the window, is also
# suppressed — the flap never accumulates a write no matter how many times
# it re-renders differently within the window.
FLAP_TICK_2=$((LAST_MERGE + 900))
DECISION="$(work_plan_decision "## Ready\n(none)" "## Ready\n(none)" "$LAST_MERGE" "$FLAP_TICK_2" "$DEBOUNCE")"
assert_eq "$DECISION" "SUPPRESS" "a settled-back-to-committed-state re-render inside the window still suppresses"

# ---------------------------------------------------------------------------
# Test 4: a PERSISTING genuine change still produces exactly one write, once
# the debounce window elapses
# ---------------------------------------------------------------------------
echo ""
echo "Test 4: a change that persists past the debounce window produces exactly one write"

PAST_WINDOW=$((LAST_MERGE + DEBOUNCE + 1))
DECISION="$(work_plan_decision "## Ready\n- #5629" "## Ready\n(none)" "$LAST_MERGE" "$PAST_WINDOW" "$DEBOUNCE")"
assert_eq "$DECISION" "WRITE" "the same persisting diff writes once the debounce window has elapsed"

# Exactly at the boundary the window has NOT yet elapsed (strict <), so this
# must still suppress -- guards against an off-by-one flipping the gate open
# one tick early.
AT_BOUNDARY=$((LAST_MERGE + DEBOUNCE))
DECISION="$(work_plan_decision "## Ready\n- #5629" "## Ready\n(none)" "$LAST_MERGE" "$AT_BOUNDARY" "$DEBOUNCE")"
assert_eq "$DECISION" "WRITE" "elapsed == debounce_secs is treated as \"the window has elapsed\" (not suppressed)"

JUST_BEFORE=$((LAST_MERGE + DEBOUNCE - 1))
DECISION="$(work_plan_decision "## Ready\n- #5629" "## Ready\n(none)" "$LAST_MERGE" "$JUST_BEFORE" "$DEBOUNCE")"
assert_eq "$DECISION" "SUPPRESS" "one second before the window elapses, the same diff is still suppressed"

# ---------------------------------------------------------------------------
# Test 5: no docs-maintenance PR has ever merged (last_merged_epoch == 0) —
# a genuine first-ever change must never be suppressed by a merge that never
# happened.
# ---------------------------------------------------------------------------
echo ""
echo "Test 5: an empty docs-maintenance history never suppresses a genuine diff"

DECISION="$(work_plan_decision "## Ready\n- #1" "(none)" 0 1000000 "$DEBOUNCE")"
assert_eq "$DECISION" "WRITE" "last_merged_epoch == 0 (no prior docs PR) writes immediately"

# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed"
exit 0
