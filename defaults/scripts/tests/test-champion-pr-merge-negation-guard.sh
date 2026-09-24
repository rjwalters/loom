#!/usr/bin/env bash
# test-champion-pr-merge-negation-guard.sh - Regression coverage for issue #1057.
#
# THE FAILURE MODE THIS GUARDS AGAINST
#
# 2AMLogic/2am#1057: PR #1051's body contained "**does not fix #909** -- ...
# #909 is left open for its owner to close or subsume; nothing here depends
# on it." -- an explicit, twice-stated intent to leave #909 open. GitHub's
# own closingIssuesReferences parser (and the Gitea word-boundary regex
# fallback) is not negation-aware: `\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b
# [[:space:]]+#N` matches "does not fix #909" exactly like "fixes #909", so
# #909 was auto-closed against the PR author's stated intent.
# champion-pr-merge.md's Step 4 ("Verify Issue Auto-Close") called
# `gh issue close` on every candidate `forge_pr_close_targets()` returned
# with no re-check of the source text for negation.
#
# THE FIX
#
# forge-helpers.sh gains forge_text_has_unnegated_closing_ref(TEXT, ISSUE_NUM)
# -- true only when TEXT has at least one closing-keyword reference to
# ISSUE_NUM that is NOT preceded by a negation word (not/never/n't-family) in
# the same clause. champion-pr-merge.md's Step 4 cross-checks every
# LINKED_ISSUES candidate against the PR body AND the squash merge commit
# message before calling `gh issue close`; a negated-only candidate is
# either left alone (still open) or reopened with an explanatory comment (if
# GitHub's own parser already closed it) -- mirroring the #4569
# partial-increment self-heal pattern. The Gitea branch of
# forge_pr_close_targets() filters through the same predicate directly,
# since it has no GraphQL ground truth to defer to and (unlike the GitHub
# branch, kept raw for merge-pr.sh's #4569 partial-increment-conflict
# detector) no consumer that depends on the raw, negation-unaware match set.
#
# This suite is hybrid, mirroring test-champion-premise-false-close.sh:
#
#   1. LOGIC -- forge_text_has_unnegated_closing_ref() is exercised directly
#      against the exact PR #1051 reproduction shape (this suite), in
#      addition to the broader table already in test-forge-helpers.sh.
#   2. WIRING -- champion-pr-merge.md's Step 4 code block and
#      forge-helpers.sh's forge_pr_close_targets() are pinned with literal
#      assert_doc_contains checks so a future edit cannot silently drop the
#      cross-check, the reopen self-heal, or the Gitea-branch filtering.
#
# Hermetic: sources forge-helpers.sh and greps shipped doc/lib files. No
# forge, no network, no tokens.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Role prompts and lib scripts are shipped (installed at .claude/commands/loom
# and .loom/scripts/lib) -- resolve the way each layout actually lays it out:
# the installed path first (consumer repos, and Loom's own dogfooded
# checkout), falling back to the defaults/ source-tree path (a bare source
# checkout with no installed copy yet). See issue #6194 / #6241.
if [[ -d "$REPO_ROOT/.claude/commands/loom" ]]; then
    ROLE_DIR="$REPO_ROOT/.claude/commands/loom"
else
    ROLE_DIR="$REPO_ROOT/defaults/.claude/commands/loom"
fi
if [[ -f "$REPO_ROOT/.loom/scripts/lib/forge-helpers.sh" ]]; then
    FORGE_HELPERS="$REPO_ROOT/.loom/scripts/lib/forge-helpers.sh"
else
    FORGE_HELPERS="$REPO_ROOT/defaults/scripts/lib/forge-helpers.sh"
fi

CHAMPION_PR_MERGE_MD="$ROLE_DIR/champion-pr-merge.md"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_doc_contains() {
    local file="$1" needle="$2" msg="$3"
    if grep -qF -- "$needle" "$file"; then
        pass "$msg"
    else
        fail "$msg (missing literal in $file: $needle)"
    fi
}

echo "================================"
echo "test-champion-pr-merge-negation-guard.sh (#1057)"
echo "================================"

# =============================================================================
# Part 1: forge_text_has_unnegated_closing_ref() against the exact #1051 shape
# =============================================================================
echo ""
echo "Part 1: forge_text_has_unnegated_closing_ref() -- exact PR #1051 reproduction"

# shellcheck source=/dev/null
source "$FORGE_HELPERS"

PR1051_BODY='## Relationship to #909 (not fixed here)

**does not fix #909** -- it structurally sidesteps that bug shape by reading
the SUMMARY line on purpose; #909 is left open for its owner to close or
subsume; nothing here depends on it.'

if ! forge_text_has_unnegated_closing_ref "$PR1051_BODY" "909"; then
    pass "PR #1051's exact body text carries NO real (unnegated) closing reference to #909"
else
    fail "PR #1051's exact body text was wrongly treated as a real closing reference to #909"
fi

if forge_text_has_unnegated_closing_ref "Closes #902" "902"; then
    pass "an ordinary 'Closes #N' trailer alongside the negated mention still closes #902"
else
    fail "an ordinary 'Closes #N' trailer was wrongly suppressed"
fi

# =============================================================================
# Part 2: WIRING -- champion-pr-merge.md's Step 4 cross-checks before closing
# =============================================================================
echo ""
echo "Part 2: champion-pr-merge.md Step 4 wiring"

assert_doc_contains "$CHAMPION_PR_MERGE_MD" "forge_text_has_unnegated_closing_ref" \
    "Step 4 calls forge_text_has_unnegated_closing_ref before gh issue close"

assert_doc_contains "$CHAMPION_PR_MERGE_MD" "gh issue reopen" \
    "Step 4 reopens an issue GitHub's own parser closed on a negated-only reference"

assert_doc_contains "$CHAMPION_PR_MERGE_MD" "MERGE_COMMIT_MSG" \
    "Step 4 cross-checks the squash merge commit message, not just the PR body"

assert_doc_contains "$CHAMPION_PR_MERGE_MD" "#1057" \
    "Step 4's negation cross-check cites issue #1057"

# The negation cross-check must run BEFORE the unconditional `gh issue close`
# call, not after -- confirm by line number rather than mere presence.
NEGATION_LINE=$(grep -n "forge_text_has_unnegated_closing_ref" "$CHAMPION_PR_MERGE_MD" | tail -1 | cut -d: -f1)
CLOSE_LINE=$(grep -n 'gh issue close "\$issue"' "$CHAMPION_PR_MERGE_MD" | tail -1 | cut -d: -f1)
if [[ -n "$NEGATION_LINE" && -n "$CLOSE_LINE" && "$NEGATION_LINE" -lt "$CLOSE_LINE" ]]; then
    pass "the negation cross-check runs BEFORE the unconditional gh issue close call"
else
    fail "the negation cross-check does not precede gh issue close (negation line=$NEGATION_LINE, close line=$CLOSE_LINE)"
fi

# =============================================================================
# Part 3: WIRING -- forge_pr_close_targets()'s Gitea branch filters directly
# =============================================================================
echo ""
echo "Part 3: forge-helpers.sh forge_pr_close_targets() Gitea-branch wiring"

assert_doc_contains "$FORGE_HELPERS" "forge_text_has_unnegated_closing_ref" \
    "forge-helpers.sh defines forge_text_has_unnegated_closing_ref"

# The Gitea branch (inside the `if [[ "$FORGE_TYPE" == "gitea" ]]; then ... fi`
# block of forge_pr_close_targets) must itself call the negation predicate --
# extract just that branch and check within it, so this assertion fails if a
# future edit moves the filtering out of the Gitea branch (e.g. into the
# GitHub branch only, which would leave Gitea vulnerable per the AC).
GITEA_BRANCH=$(awk '
  /^forge_pr_close_targets\(\)/ { infn = 1 }
  infn && /FORGE_TYPE.*==.*gitea/ { ingitea = 1 }
  infn && ingitea && /else/ { exit }
  infn && ingitea { print }
' "$FORGE_HELPERS")

if printf '%s' "$GITEA_BRANCH" | grep -q "forge_text_has_unnegated_closing_ref"; then
    pass "forge_pr_close_targets()'s Gitea branch filters through forge_text_has_unnegated_closing_ref"
else
    fail "forge_pr_close_targets()'s Gitea branch does not call forge_text_has_unnegated_closing_ref"
fi

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
