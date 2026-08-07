#!/usr/bin/env bash
# test-classify-error.sh - Unit tests for lib/classify-error.sh (#5631)
#
# Table-driven coverage of `classify_error`'s TOKEN_EXHAUSTED / SESSION_LIMIT
# regexes against known Claude CLI limit strings, per the "Suggested test"
# spec in issue #5631: session, weekly, monthly usage, monthly spend,
# per-model, plus the SESSION_LIMIT precedence edge case that motivated the
# ordering in the first place (#3947).
#
# Usage:
#   ./.loom/scripts/tests/test-classify-error.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
SRC="$SCRIPTS_DIR/lib/classify-error.sh"

# shellcheck source=../lib/classify-error.sh
source "$SRC"

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

echo "--- classify_error: TOKEN_EXHAUSTED phrase family (#5631) ---"

# Table: description | CLI output | expected category
while IFS='|' read -r desc output expected; do
    [[ -z "$desc" ]] && continue
    actual="$(classify_error "$output" 1)"
    assert_eq "$expected" "$actual" "$desc"
done <<'EOF'
session limit|You've hit your session limit|TOKEN_EXHAUSTED
weekly limit|You've hit your weekly limit|TOKEN_EXHAUSTED
monthly usage limit|You've hit your monthly usage limit|TOKEN_EXHAUSTED
monthly spend limit (issue #5631 regression)|You've hit your monthly spend limit - raise it at claude.ai/settings/usage|TOKEN_EXHAUSTED
bare limit, no filler words|You've hit your limit|TOKEN_EXHAUSTED
per-model ceiling (#4501)|You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.|TOKEN_EXHAUSTED
out of extra usage|You are out of extra usage for this billing period|TOKEN_EXHAUSTED
used 100% of weekly limit|You have used 100% of your weekly limit|RECOVERABLE
EOF

echo
echo "--- classify_error: SESSION_LIMIT precedence is unaffected by the widened TOKEN_EXHAUSTED regex (#3947) ---"

assert_eq "SESSION_LIMIT" "$(classify_error "maximum number of concurrent sessions reached" 1)" \
    "concurrent-session capacity fault stays SESSION_LIMIT, not TOKEN_EXHAUSTED"
assert_eq "SESSION_LIMIT" "$(classify_error "Another session is already active" 1)" \
    "'another session is already active' stays SESSION_LIMIT"

echo
echo "--- classification_is_transient: TOKEN_EXHAUSTED stays retryable (rotation path consumes it first) ---"

if classification_is_transient "TOKEN_EXHAUSTED"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: TOKEN_EXHAUSTED is transient (retry/rotate, not fatal)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: TOKEN_EXHAUSTED is transient (retry/rotate, not fatal)"
fi

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
