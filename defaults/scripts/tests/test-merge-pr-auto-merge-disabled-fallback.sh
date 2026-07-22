#!/usr/bin/env bash
# test-merge-pr-auto-merge-disabled-fallback.sh - Unit tests for the repo-level
# "Allow auto-merge disabled" fallback in merge-pr.sh (#3763).
#
# When a repository's "Allow auto-merge" setting is OFF, GitHub rejects the
# enablePullRequestAutoMerge mutation with the error string:
#
#     gh: Auto merge is not allowed for this repository
#
# This is a STATIC, repo-level condition — distinct from the PR-state-level
# "is in clean status" (#3371) and "is in unstable status" (#3486/#3664/#3678)
# rejections that merge-pr.sh already special-cases. Before #3763 the new
# rejection matched neither existing grep, so the script fell through to the
# generic terminal error and aborted — even when the PR was immediately
# mergeable (the reported failure: a CLEAN, Judge-approved PR that could have
# merged synchronously).
#
# The #3763 fallback matches the new error string and then re-checks the PR's
# mergeability with a fresh (uncached) fetch:
#   - .mergeable == "true"  -> flip to the synchronous immediate-merge path
#                              (AUTO_MERGE=false, AUTO_MERGE_OK=true, break).
#   - otherwise             -> preserve the existing terminal error (no silent
#                              bypass of a genuine merge blocker).
#
# This test exercises two surfaces:
#   1. The trigger-string matcher (grep) is correct and mutually exclusive with
#      the CLEAN/UNSTABLE matchers — it fires only on the auto-merge-disabled
#      error and never on the clean/unstable fixtures (and vice versa).
#   2. The mergeability-gated decision policy: fires only when .mergeable is
#      "true", preserves the terminal error otherwise. We replicate the same
#      predicate shape merge-pr.sh uses so the script and the test stay in
#      lockstep, plus assert the script source actually wires the fallback.
#
# Usage:
#   ./.loom/scripts/tests/test-merge-pr-auto-merge-disabled-fallback.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MERGE_PR_SRC="$HELPERS_DIR/merge-pr.sh"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1"
    local actual="$2"
    local msg="$3"
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

# --- The exact gh error string surfaced by the repo-level toggle ---
# Mirrors the failure in issue #3763's Context block. The `gh` prefix and the
# surrounding loom-auto-merge WARNING wrapper are included to prove the
# substring matcher is robust to the real, decorated output.
disabled_error="WARNING: Failed to enable auto-merge for PR #26: gh: Auto merge is not allowed for this repository"
clean_error="gh: Pull request Pull request is in clean status (enablePullRequestAutoMerge)"
unstable_error="gh: Pull request Pull request is in unstable status (enablePullRequestAutoMerge)"

# --- Test the trigger-string matcher (mutual exclusivity) ---
echo "Testing the auto-merge-disabled substring matcher shape (#3763)..."

if echo "$disabled_error" | grep -q "Auto merge is not allowed for this repository"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: 'Auto merge is not allowed for this repository' matches the gh error"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: substring matcher missed the auto-merge-disabled error"
fi

# The new matcher must NOT fire on the CLEAN or UNSTABLE fixtures.
if echo "$clean_error" | grep -q "Auto merge is not allowed for this repository"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: auto-merge-disabled matcher fired on the CLEAN error (false positive)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: auto-merge-disabled matcher does NOT match the CLEAN error"
fi

if echo "$unstable_error" | grep -q "Auto merge is not allowed for this repository"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: auto-merge-disabled matcher fired on the UNSTABLE error (false positive)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: auto-merge-disabled matcher does NOT match the UNSTABLE error"
fi

# Conversely, the existing CLEAN/UNSTABLE matchers must NOT fire on the new
# auto-merge-disabled error — the three fallbacks stay mutually exclusive on
# their trigger strings.
if echo "$disabled_error" | grep -q "is in clean status"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: CLEAN matcher fired on the auto-merge-disabled error (false positive)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: CLEAN matcher does NOT match the auto-merge-disabled error"
fi

if echo "$disabled_error" | grep -q "is in unstable status"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: UNSTABLE matcher fired on the auto-merge-disabled error (false positive)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: UNSTABLE matcher does NOT match the auto-merge-disabled error"
fi

# --- Test the mergeability-gated decision policy ---
# Mirror the merge-pr.sh callsite predicate: after the error string matches,
# the fallback fires (flips to the synchronous merge path) only when the
# fresh, uncached PR fetch reports .mergeable == "true". Anything else
# (false, or null/empty because GitHub has not computed it yet) preserves the
# terminal error — no silent bypass of a genuine merge blocker.
echo ""
echo "Testing the auto-merge-disabled mergeability policy (#3763)..."

# Returns "merge" when the fallback fires (flip to immediate merge), else
# "preserve" (keep the existing terminal error).
_amd_decision() {
    local mergeable="$1"
    if [[ "$mergeable" == "true" ]]; then
        echo "merge"
    else
        echo "preserve"
    fi
}

# Extract `.mergeable // empty` from a REST PR JSON payload exactly as the
# script does, so the fixtures exercise the real jq expression.
_mergeable_of() {
    echo "$1" | jq -r '.mergeable // empty'
}

# Core #3763 case: auto-merge disabled + PR immediately mergeable -> fallback
# fires (synchronous immediate merge).
pr_mergeable='{"number":26,"mergeable":true,"merged":false}'
assert_eq "true" "$(_mergeable_of "$pr_mergeable")" "#3763: mergeable PR JSON yields .mergeable == true"
assert_eq "merge" "$(_amd_decision "$(_mergeable_of "$pr_mergeable")")" \
  "#3763: auto-merge disabled + mergeable PR -> fallback fires (immediate merge)"

# Not-mergeable case: PR has a conflict/blocker -> preserve the terminal error.
# NOTE: jq's `.mergeable // empty` collapses boolean `false` to empty (the `//`
# alternative operator treats `false` like `null`). The script keys strictly off
# `== "true"`, so `false` -> "" -> preserve is exactly correct; only an explicit
# `true` ever triggers the fallback. We mirror that collapse here so the fixture
# exercises the real predicate the script uses.
pr_conflicting='{"number":26,"mergeable":false,"merged":false}'
assert_eq "" "$(_mergeable_of "$pr_conflicting")" "#3763: conflicting PR (.mergeable=false) collapses to empty under '// empty'"
assert_eq "preserve" "$(_amd_decision "$(_mergeable_of "$pr_conflicting")")" \
  "#3763: auto-merge disabled + NOT mergeable -> preserve terminal error (no bypass)"

# Mergeability unknown (GitHub has not computed .mergeable yet -> null) ->
# preserve the terminal error rather than merging blind.
pr_unknown='{"number":26,"merged":false}'
assert_eq "" "$(_mergeable_of "$pr_unknown")" "#3763: PR JSON without .mergeable yields empty"
assert_eq "preserve" "$(_amd_decision "$(_mergeable_of "$pr_unknown")")" \
  "#3763: auto-merge disabled + mergeable unknown (null) -> preserve (do not merge blind)"

# Explicit null is equivalent to unknown -> preserve.
pr_null='{"number":26,"mergeable":null,"merged":false}'
assert_eq "" "$(_mergeable_of "$pr_null")" "#3763: explicit null .mergeable yields empty"
assert_eq "preserve" "$(_amd_decision "$(_mergeable_of "$pr_null")")" \
  "#3763: auto-merge disabled + .mergeable null -> preserve"

# --- Assert merge-pr.sh source actually wires the #3763 fallback ---
# A refactor that drops the grep, the mergeability re-check, or the
# fall-through flip must fail this test.
echo ""
echo "Testing merge-pr.sh source wiring (#3763)..."

if grep -q 'grep -q "Auto merge is not allowed for this repository"' "$MERGE_PR_SRC"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: merge-pr.sh greps for the auto-merge-disabled error string"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: merge-pr.sh missing the auto-merge-disabled grep (#3763 regression)"
fi

# The fallback must re-check mergeability via a fresh uncached fetch before
# flipping to the synchronous merge path.
if grep -q '_AMD_MERGEABLE' "$MERGE_PR_SRC" && grep -q '_AMD_RECHECK_JSON' "$MERGE_PR_SRC"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: merge-pr.sh re-checks PR mergeability (_AMD_MERGEABLE) before falling back"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: merge-pr.sh missing the mergeability re-check for #3763"
fi

# The re-check must use the uncached fetch helper (fresh state), matching the
# CLEAN/no-required-checks fallbacks.
# Anchor on the actual `grep -q "..."` code lines, not the bare error strings:
# those strings also appear in the surrounding comment block, so anchoring on
# them would capture only comments and miss the implementation.
_amd_block="$(awk '/grep -q "Auto merge is not allowed for this repository"/{f=1} f; /grep -q "is in clean status"/{exit}' "$MERGE_PR_SRC")"
if echo "$_amd_block" | grep -q 'forge_get_pr_nocache'; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: #3763 fallback re-fetches PR state via forge_get_pr_nocache (uncached)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: #3763 fallback must re-fetch via forge_get_pr_nocache"
fi

# The fallback must flip to the synchronous merge path on the mergeable branch
# (AUTO_MERGE=false, AUTO_MERGE_OK=true) — the same flip the CLEAN/UNSTABLE
# fallbacks use.
if echo "$_amd_block" | grep -q 'AUTO_MERGE=false' && echo "$_amd_block" | grep -q 'AUTO_MERGE_OK=true'; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: #3763 fallback flips to the synchronous-merge path (AUTO_MERGE=false, AUTO_MERGE_OK=true)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: #3763 fallback must flip AUTO_MERGE=false / AUTO_MERGE_OK=true"
fi

# The not-mergeable branch must preserve the terminal error. Assert the
# fallback block still contains the terminal `error` call so a genuine blocker
# is not silently bypassed.
if echo "$_amd_block" | grep -q 'error "Failed to enable auto-merge for PR #\$PR_NUMBER: \$AUTO_MERGE_OUTPUT"'; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: #3763 fallback preserves the terminal error when the PR is not mergeable"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: #3763 fallback must preserve the terminal error on the not-mergeable branch"
fi

# Ordering: the #3763 fallback must be inserted BEFORE the CLEAN/UNSTABLE greps
# (the auto-merge-disabled rejection matches neither, so it must be caught
# first, mirroring the #3720 no-required-checks fallback placement).
_amd_line=$(grep -n 'grep -q "Auto merge is not allowed for this repository"' "$MERGE_PR_SRC" | head -1 | cut -d: -f1)
_clean_line=$(grep -n 'grep -q "is in clean status"' "$MERGE_PR_SRC" | head -1 | cut -d: -f1)
if [[ -n "$_amd_line" ]] && [[ -n "$_clean_line" ]] && [[ "$_amd_line" -lt "$_clean_line" ]]; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: #3763 fallback is inserted BEFORE the clean/unstable greps"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: #3763 fallback must precede the clean/unstable greps (amd=$_amd_line clean=$_clean_line)"
fi

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
