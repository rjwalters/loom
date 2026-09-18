#!/usr/bin/env bash
# test-merge-pr-verdict-label-guard.sh - Unit tests for the PRE-merge
# verdict-label contradiction guard in merge-pr.sh (#8112).
#
# Two concurrent Judge passes on the same PR head can reach different
# verdicts roughly a minute apart (observed on PR #8076), leaving a PR
# carrying BOTH `loom:pr` (approved) and a blocking/contradicting label
# (`loom:changes-requested`, `loom:blocked`, `loom:operator`, or
# `loom:review-requested`) simultaneously. `_check_loom_pr_label` only
# checks `loom:pr`'s ABSENCE (#7419); it has no way to see a contradicting
# label standing beside a PRESENT `loom:pr`. `_check_verdict_label_
# contradiction` closes that gap: it hard-blocks the merge (error, exit 1)
# naming BOTH offending labels, `--dry-run` reports the would-be block
# without exiting 1, and there is deliberately no bypass flag (not even
# `--allow-unapproved`, which covers a different act — see merge-pr.sh's own
# comment above the guard).
#
# The decision logic itself (which label pairs count as a contradiction) is
# in lib/label-preflight.sh's loom_verdict_label_contradiction_message() and
# is covered independently by test-label-preflight.sh; this suite exercises
# the merge-pr.sh-side wiring (dry-run behavior, hard-block behavior, and
# that no override flag exists).
#
# Strategy (mirrors test-merge-pr-loom-pr-label-guard.sh): extract
# _check_verdict_label_contradiction from the real merge-pr.sh source and
# source it (plus the real label-preflight.sh lib, unstubbed — its logic is
# pure string matching with no forge calls), then assert on exit code +
# emitted message. The guard calls `error` (which `exit 1`s), so it is always
# invoked inside a command-substitution subshell.
#
# Usage:
#   ./.loom/scripts/tests/test-merge-pr-verdict-label-guard.sh

# SC2034: PR_NUMBER/PR_LABELS/PR_HEAD_SHA/DRY_RUN are read only by the
# extracted+sourced function, which shellcheck cannot see.
# shellcheck disable=SC2034

set -euo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$TEST_DIR/.." && pwd)"
MERGE_PR_SRC="$HELPERS_DIR/merge-pr.sh"
LABEL_PREFLIGHT_LIB="$HELPERS_DIR/lib/label-preflight.sh"

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
    # Here-string, not a pipe (see test-merge-pr-loom-pr-label-guard.sh's
    # identical rationale, #3820).
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

assert_not_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if ! grep -qF -- "$needle" <<<"$haystack"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Unexpected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

# --- Minimal logging/error shims the extracted function calls ---
info()    { echo "INFO: $*"; }
success() { echo "OK: $*"; }
warning() { echo "WARN: $*" >&2; }
error()   { echo "ERROR: $*" >&2; exit 1; }

if [[ ! -f "$LABEL_PREFLIGHT_LIB" ]]; then
    echo -e "${RED}FATAL${NC}: $LABEL_PREFLIGHT_LIB not found" >&2
    exit 2
fi
# shellcheck source=/dev/null
source "$LABEL_PREFLIGHT_LIB"

# --- Extract the function under test from merge-pr.sh and source it ---
# From `_check_verdict_label_contradiction() {` up to (not including) the
# following `_check_verdict_label_contradiction` invocation line — the
# function is written as a single dense line (file-size-ratchet offset, see
# merge-pr.sh's own comment above it), so the extraction captures exactly
# that one line. Extracting from source keeps the test in lockstep with the
# script instead of re-implementing it.
FUNCS_FILE="$(mktemp)"
trap 'rm -f "$FUNCS_FILE" 2>/dev/null || true' EXIT
awk '/^_check_verdict_label_contradiction\(\) \{/ { print; exit }' "$MERGE_PR_SRC" > "$FUNCS_FILE"

if ! grep -q '_check_verdict_label_contradiction()' "$FUNCS_FILE"; then
    echo -e "${RED}FATAL${NC}: could not extract _check_verdict_label_contradiction from $MERGE_PR_SRC" >&2
    exit 2
fi
# shellcheck disable=SC1090
source "$FUNCS_FILE"

# --- Shared globals the function reads ---
PR_NUMBER="8076"
PR_LABELS=""
PR_HEAD_SHA="abc1234"
DRY_RUN=false

LAST_OUT=""
LAST_RC=0
run_guard() {
    set +e
    LAST_OUT="$( _check_verdict_label_contradiction 2>&1 )"
    LAST_RC=$?
    set -e
}

echo "Testing _check_verdict_label_contradiction behavior..."

# T1: loom:pr + loom:changes-requested -> hard block (exit 1) naming BOTH labels.
DRY_RUN=false
PR_LABELS=$'loom:pr\nloom:changes-requested'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "1" "$LAST_RC" "loom:pr + loom:changes-requested -> merge hard-blocked (exit 1)"
assert_contains "$LAST_OUT" "Merge blocked" "Block message is emitted"
assert_contains "$LAST_OUT" "loom:pr" "Block message names loom:pr"
assert_contains "$LAST_OUT" "loom:changes-requested" "Block message names the contradicting label"
assert_contains "$LAST_OUT" "deadbeef" "Block message prints the current head SHA"

# T2: same contradiction, labels in the OPPOSITE order -> still detected and
# blocked (order independence: the guard walks a fixed list of blocking
# labels, never "whichever label the forge happens to return first").
DRY_RUN=false
PR_LABELS=$'loom:changes-requested\nloom:pr'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "1" "$LAST_RC" "Reversed label order -> still hard-blocked (order-independent)"
assert_contains "$LAST_OUT" "loom:changes-requested" "Reversed order -> message still names the contradicting label"

# T3: loom:pr + loom:blocked -> hard block.
DRY_RUN=false
PR_LABELS=$'loom:pr\nloom:blocked'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "1" "$LAST_RC" "loom:pr + loom:blocked -> merge hard-blocked (exit 1)"
assert_contains "$LAST_OUT" "loom:blocked" "Block message names loom:blocked"

# T4: loom:pr + loom:operator -> hard block.
DRY_RUN=false
PR_LABELS=$'loom:pr\nloom:operator'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "1" "$LAST_RC" "loom:pr + loom:operator -> merge hard-blocked (exit 1)"
assert_contains "$LAST_OUT" "loom:operator" "Block message names loom:operator"

# T5: loom:pr alone (the overwhelmingly common case) -> guard is a no-op.
DRY_RUN=false
PR_LABELS=$'loom:pr'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "0" "$LAST_RC" "loom:pr alone -> guard passes (exit 0)"
assert_not_contains "$LAST_OUT" "Merge blocked" "loom:pr alone -> no block message"

# T6: loom:changes-requested WITHOUT loom:pr -> guard is a no-op (the
# existing _check_loom_pr_label guard, not this one, handles a missing
# loom:pr — this guard only fires on an actual CONTRADICTION).
DRY_RUN=false
PR_LABELS=$'loom:changes-requested'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "0" "$LAST_RC" "loom:changes-requested without loom:pr -> this guard is a no-op"
assert_not_contains "$LAST_OUT" "Merge blocked" "No loom:pr present -> no block message from this guard"

# T7: no labels at all -> guard is a no-op.
DRY_RUN=false
PR_LABELS=""
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "0" "$LAST_RC" "Empty label set -> guard passes (exit 0)"

# T8: contradiction + --dry-run -> warning printed, exits 0, no hard block.
DRY_RUN=true
PR_LABELS=$'loom:pr\nloom:changes-requested'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "0" "$LAST_RC" "--dry-run + contradiction -> guard does NOT exit 1 (dry-run contract)"
assert_contains "$LAST_OUT" "[dry-run] Would BLOCK" "--dry-run -> reports the would-be block"
assert_contains "$LAST_OUT" "loom:changes-requested" "--dry-run message still names the contradicting label"
DRY_RUN=false

# T9: loom:pr + loom:review-requested -> also treated as a contradiction
# (generalizing #4570 the same way Champion's Verdict-State Janitor Part 1
# does, #7018 — a stray loom:pr beside an active/pending re-review).
DRY_RUN=false
PR_LABELS=$'loom:pr\nloom:review-requested'
PR_HEAD_SHA="deadbeef"
run_guard
assert_eq "1" "$LAST_RC" "loom:pr + loom:review-requested -> merge hard-blocked (exit 1)"
assert_contains "$LAST_OUT" "loom:review-requested" "Block message names loom:review-requested"

# --- Source-contains guards (fail if a refactor drops the key behavior) ---
echo ""
echo "Testing merge-pr.sh source guards..."
src="$(cat "$MERGE_PR_SRC")"
assert_contains "$src" "_check_verdict_label_contradiction" \
  "merge-pr.sh defines and invokes _check_verdict_label_contradiction"
assert_contains "$src" "lib/label-preflight.sh" \
  "merge-pr.sh sources label-preflight.sh"
assert_not_contains "$src" '"$1" == "--allow-verdict-contradiction"' \
  "no bypass flag exists for this guard (deliberate — see merge-pr.sh's comment above the guard)"

# Assert the guard is invoked BEFORE the auto-merge path (line ordering),
# same convention as test-merge-pr-loom-pr-label-guard.sh.
guard_match="$(grep -n '^_check_verdict_label_contradiction$' "$MERGE_PR_SRC" || true)"
guard_line="${guard_match%%$'\n'*}"
guard_line="${guard_line%%:*}"
automerge_match="$(grep -n '^# Handle auto-merge mode' "$MERGE_PR_SRC" || true)"
automerge_line="${automerge_match%%$'\n'*}"
automerge_line="${automerge_line%%:*}"
if [[ -n "$guard_line" && -n "$automerge_line" && "$guard_line" -lt "$automerge_line" ]]; then
    ordered="yes"
else
    ordered="no (guard=$guard_line automerge=$automerge_line)"
fi
assert_eq "yes" "$ordered" \
  "guard is invoked before both merge paths (before '# Handle auto-merge mode')"

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
