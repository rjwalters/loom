#!/usr/bin/env bash
# test-champion-promotion-throughput.sh - Regression test for issue #10753
# item 1 (merges must not starve promotion) and for #11512, which retired the
# tier caps #10753 item 2 made configurable.
#
# THE FAILURE MODES THIS GUARDS AGAINST
#
#   pass order    champion.md ran promotion only "if no PRs remain". A repo
#                 with a held PR queue never reached it (rjwalters/loom on
#                 2026-10-07: 30 of 38 `loom:pr` PRs held by loom:operator).
#   stale caps    The per-tier caps and the Tier 3 backlog cap read `tier:*`
#                 labels, which #11103 retired with no mapping. Promotion is
#                 ordered by `loom:very-important` / `loom:important` instead,
#                 and no Champion prompt may read the old labels or caps.
#
# WHAT THIS SUITE DOES
#
#   1. ORDER -- the Promotion Order prose ranks by level then age with no
#      cap, and no Champion file names the retired caps or tier labels.
#   2. DOCS -- promotion-throughput.md documents the live knobs and the
#      retirement.
#   3. WIRING -- champion.md runs promotion every pass with the two slice
#      knobs.
#
# Hermetic: file reads only. No forge, no network. No `set -o pipefail` on purpose (#7790).

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Shipped files: resolve the installed layout first (consumer repos, and
# Loom's own dogfooded checkout), falling back to the defaults/ source tree
# (#6194 / #6241).
pick_dir() { if [[ -d "$REPO_ROOT/$1" ]]; then echo "$REPO_ROOT/$1"; else echo "$REPO_ROOT/$2"; fi; }
ROLE_DIR="$(pick_dir .claude/commands/loom defaults/.claude/commands/loom)"
AGENT_DIR="$(pick_dir .claude/agents defaults/.claude/agents)"
DOCS_DIR="$(pick_dir .loom/docs defaults/docs)"

CHAMPION_MD="$ROLE_DIR/champion.md"
PROMO_MD="$ROLE_DIR/champion-issue-promo.md"
COMMON_MD="$ROLE_DIR/champion-common.md"
REF_MD="$ROLE_DIR/champion-reference.md"
PR_MERGE_MD="$ROLE_DIR/champion-pr-merge.md"
AGENT_MD="$AGENT_DIR/loom-champion.md"
THROUGHPUT_DOC="$DOCS_DIR/promotion-throughput.md"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_contains() {
    if [[ "$1" == *"$2"* ]]; then pass "$3"; else fail "$3"; echo "    Expected to contain: '$2'"; echo "    Actual: '$1'"; fi
}
assert_not_contains() {
    if [[ "$1" != *"$2"* ]]; then pass "$3"; else fail "$3"; echo "    Expected NOT to contain: '$2'"; fi
}
assert_doc_contains() {
    if grep -qF -- "$2" "$1"; then pass "$3"; else fail "$3 (missing literal in $1: $2)"; fi
}
assert_doc_lacks() {
    if grep -qF -- "$2" "$1"; then fail "$3 (found literal in $1: $2)"; else pass "$3"; fi
}

# Print one section, from a heading containing $2 up to the next heading of
# the same or shallower level. Fenced code is skipped when looking for
# headings, so a shell comment is never mistaken for one.
section_body() {
    awk -v want="$2" '
        /^```/ { fence = !fence }
        !fence && /^#+ / {
            n = 0
            while (substr($0, n + 1, 1) == "#") n++
            if (inside && n <= lvl) inside = 0
            if (!inside && index($0, want) > 0) { inside = 1; lvl = n }
        }
        inside { print }
    ' "$1"
}

echo "================================"
echo "test-champion-promotion-throughput.sh (#10753)"
echo "================================"

for f in "$CHAMPION_MD" "$PROMO_MD" "$COMMON_MD" "$REF_MD" "$PR_MERGE_MD" "$AGENT_MD" "$THROUGHPUT_DOC"; do
    if [[ ! -f "$f" ]]; then
        echo "FATAL: shipped file not found: $f" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
echo ""
echo "Test 1: promotion is ordered by priority level; the tier caps are retired (#11512)"
ORDER="$(section_body "$PROMO_MD" "Promotion Order")"
assert_contains "$ORDER" "\`loom:very-important\`, then \`loom:important\`" \
    "Promotion Order ranks very-important, then important"
assert_contains "$ORDER" "oldest first within a level" "Promotion Order breaks ties oldest first"
assert_contains "$ORDER" "There is no per-level cap" "Promotion Order promotes every passing proposal"
assert_contains "$(section_body "$PROMO_MD" "Setting Priority")" "label the blocker too" \
    "Champion labels a priority proposal's blocker itself (no daemon inheritance)"
for f in "$CHAMPION_MD" "$PROMO_MD" "$COMMON_MD" "$REF_MD" "$AGENT_MD"; do
    for lit in "LOOM_CHAMPION_TIER" "tier:goal" "tier:maintenance" "classify-capacity-defer" "Rate Limiting by Tier"; do
        assert_doc_lacks "$f" "$lit" "$(basename "$f") no longer names '$lit'"
    done
done

# ---------------------------------------------------------------------------
echo ""
echo "Test 2: promotion-throughput.md documents the live knobs and the retirement"
for v in LOOM_CHAMPION_PR_SLICE LOOM_CHAMPION_PROMOTION_SLICE; do
    assert_doc_contains "$THROUGHPUT_DOC" "\`$v\`" "promotion-throughput.md documents $v"
done
assert_doc_contains "$THROUGHPUT_DOC" "retired" "promotion-throughput.md records the retired tier caps"
assert_doc_contains "$THROUGHPUT_DOC" "loom-daemon-start.sh" \
    "promotion-throughput.md says where a fleet sets the knobs"

# ---------------------------------------------------------------------------
echo ""
echo "Test 3: promotion runs every pass, even while PRs remain"
assert_doc_lacks "$CHAMPION_MD" "If no PRs need merging" "champion.md Priority 2 no longer waits for an empty PR queue"
assert_doc_lacks "$CHAMPION_MD" "If no PRs remain" "champion.md's autonomous steps no longer wait for an empty PR queue"
assert_doc_lacks "$COMMON_MD" "If no PRs, check" "champion-common.md no longer waits for an empty PR queue"
AUTO="$(section_body "$CHAMPION_MD" "Autonomous Operation")"
# shellcheck disable=SC2016  # the literal, unexpanded text is what the prompt must carry
assert_contains "$AUTO" '${LOOM_CHAMPION_PR_SLICE:-10}' "the PR pass pauses after LOOM_CHAMPION_PR_SLICE rows (default 10)"
# shellcheck disable=SC2016  # literal, as above
assert_contains "$AUTO" '${LOOM_CHAMPION_PROMOTION_SLICE:-3}' "promotion is bounded by LOOM_CHAMPION_PROMOTION_SLICE (default 3) while rows wait"
assert_contains "$AUTO" "whether or not PRs remain" "the promotion step does not depend on the PR queue emptying"
assert_contains "$AUTO" "Resume Priority 1 until every row is visited" "the PR pass still visits every row (no merge cap)"
BATCH="$(section_body "$PR_MERGE_MD" "PR Auto-Merge Batch Processing")"
assert_contains "$BATCH" "LOOM_CHAMPION_PR_SLICE" "champion-pr-merge.md's batch loop names the promotion pause"
assert_contains "$BATCH" "drain the full queue" "champion-pr-merge.md still drains the full queue"

# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed."
