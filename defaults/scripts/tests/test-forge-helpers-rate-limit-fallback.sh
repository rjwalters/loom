#!/usr/bin/env bash
# test-forge-helpers-rate-limit-fallback.sh - Unit tests for the GraphQL-
# exhaustion REST fallback helpers in lib/forge-helpers.sh (#4856).
#
# `gh issue edit`, `gh issue comment`, `gh issue reopen`, and `gh pr comment`
# are GraphQL-backed mutations. During a long sweep, GraphQL quota (5000/hr,
# shared across every agent + tool) can exhaust while REST quota still has
# headroom -- the same independent-quota fact the read-side fallback in
# check-duplicate.sh and merge-pr.sh's #4447 auto-merge-enable fallback
# already rely on. Before this fix, merge-pr.sh's best-effort mutating call
# sites (partial-increment label reset, premature-auto-close reopen,
# stacked-child deferral comment) simply swallowed a rate-limit rejection
# with the same generic warning as any other failure, silently dropping the
# label/comment update instead of retrying over REST.
#
# This file tests:
#   1. is_rate_limit_error() fires on all five documented signatures and
#      stays silent on unrelated failures (auth, network, 404).
#   2. forge_gh_comment_rl_safe / forge_gh_reopen_issue_rl_safe /
#      forge_gh_swap_label_rl_safe: succeed via the primary `gh` mutation
#      when it works, fall back to the correct REST endpoint/method only on
#      a rate-limit rejection, and propagate (not swallow) any OTHER
#      failure without attempting a REST call.
#   3. merge-pr.sh source wiring: the four mutating call sites this issue
#      is about actually route through the new wrappers.
#
# Usage:
#   ./.loom/scripts/tests/test-forge-helpers-rate-limit-fallback.sh

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

# shellcheck source=../lib/forge-helpers.sh
source "$HELPERS_DIR/lib/forge-helpers.sh"

# --- 1. is_rate_limit_error() signature table ---
echo "Testing is_rate_limit_error() signature table..."

positive_fixtures=(
    "graphql_already_exceeded:GraphQL: API rate limit already exceeded for user ID 12345"
    "rest_exceeded:HTTP 403: API rate limit exceeded for installation ID 1"
    "secondary_rate_limit:You have exceeded a secondary rate limit. Please retry your request again later."
    "abuse_detection:You have triggered an abuse detection mechanism and have been temporarily blocked."
    "submitted_too_quickly:This endpoint has hit a secondary limit and was submitted too quickly"
)

for entry in "${positive_fixtures[@]}"; do
    fixture_name="${entry%%:*}"
    fixture_value="${entry#*:}"
    TESTS_RUN=$((TESTS_RUN + 1))
    if is_rate_limit_error "$fixture_value"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: is_rate_limit_error fires on $fixture_name"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: is_rate_limit_error missed $fixture_name ('$fixture_value')"
    fi
done

negative_fixtures=(
    "auth_failure:gh: To use GitHub CLI in a GitHub Actions workflow, set the GH_TOKEN environment variable. HTTP 401 Bad credentials"
    "not_found:HTTP 404: Not Found"
    "network_error:dial tcp: lookup api.github.com: no such host"
    "bad_label:HTTP 422: Validation Failed (label does not exist)"
)

for entry in "${negative_fixtures[@]}"; do
    fixture_name="${entry%%:*}"
    fixture_value="${entry#*:}"
    TESTS_RUN=$((TESTS_RUN + 1))
    if is_rate_limit_error "$fixture_value"; then
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: is_rate_limit_error fired on $fixture_name (false positive)"
    else
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: is_rate_limit_error does NOT fire on $fixture_name"
    fi
done

# Case-insensitivity, mirroring the Rust RATE_LIMIT_SIGNATURES / check-duplicate.sh original.
TESTS_RUN=$((TESTS_RUN + 1))
if is_rate_limit_error "GRAPHQL: API RATE LIMIT ALREADY EXCEEDED"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: is_rate_limit_error matches case-insensitively"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: is_rate_limit_error must match case-insensitively"
fi

# --- 2. REST-fallback wrapper functions, with a stubbed `gh` on PATH ---
echo ""
echo "Testing forge_gh_*_rl_safe REST-fallback wrappers..."

STUB_DIR=$(mktemp -d)
ARGV_LOG="$STUB_DIR/argv.log"
GH_MODE_FILE="$STUB_DIR/mode.txt"
# The stub script below runs as a separate process, so these must be
# exported for it to see them (a plain `VAR=x cmd` prefix only exports for
# the immediate command, not variables set earlier in this shell).
export ARGV_LOG GH_MODE_FILE

# A `gh` stub whose behavior is driven by $GH_MODE_FILE:
#   "ok"          - the primary (non-`api`) mutation succeeds immediately.
#   "ratelimited" - the primary mutation is rate-limited; `gh api ...` (the
#                   REST fallback) succeeds.
#   "other-error" - the primary mutation fails with an unrelated error; `gh
#                   api ...` must NEVER be reached (recorded as a FAIL marker
#                   in the argv log if it is).
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
mode="$(cat "$GH_MODE_FILE" 2>/dev/null || echo ok)"
printf '%s\n' "$*" >> "$ARGV_LOG"

if [[ "$1" == "api" ]]; then
  if [[ "$mode" == "ratelimited" ]]; then
    exit 0
  fi
  # Reached only when it should not have been.
  echo "UNEXPECTED_REST_CALL" >> "$ARGV_LOG"
  exit 1
fi

case "$mode" in
  ok) exit 0 ;;
  ratelimited)
    echo "GraphQL: API rate limit already exceeded for user ID 1" >&2
    exit 1
    ;;
  other-error)
    echo "HTTP 404: Not Found" >&2
    exit 1
    ;;
esac
STUB
chmod +x "$STUB_DIR/gh"

_run_stubbed() {
    local mode="$1"; shift
    echo "$mode" > "$GH_MODE_FILE"
    : > "$ARGV_LOG"
    PATH="$STUB_DIR:$PATH" "$@"
}

# --- forge_gh_comment_rl_safe ---
rc=0
_run_stubbed ok forge_gh_comment_rl_safe "owner/repo" "42" "hello" >/dev/null 2>&1 || rc=$?
assert_eq "0" "$rc" "forge_gh_comment_rl_safe: primary gh issue comment succeeds -> no REST call"
TESTS_RUN=$((TESTS_RUN + 1))
if ! grep -q "^api " "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_comment_rl_safe (ok mode) never calls gh api"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_comment_rl_safe (ok mode) unexpectedly called gh api"
fi

rc=0
_run_stubbed ratelimited forge_gh_comment_rl_safe "owner/repo" "42" "hello" >/dev/null 2>&1 || rc=$?
assert_eq "0" "$rc" "forge_gh_comment_rl_safe: rate-limited primary call recovers via REST fallback"
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "^api repos/owner/repo/issues/42/comments" "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_comment_rl_safe REST fallback hits the correct comments endpoint"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_comment_rl_safe REST fallback endpoint wrong or missing"
fi

rc=0
_run_stubbed other-error forge_gh_comment_rl_safe "owner/repo" "42" "hello" >/dev/null 2>&1 || rc=$?
assert_eq "1" "$rc" "forge_gh_comment_rl_safe: non-rate-limit failure propagates (not swallowed)"
TESTS_RUN=$((TESTS_RUN + 1))
if ! grep -q "UNEXPECTED_REST_CALL" "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_comment_rl_safe never retries over REST on a non-rate-limit failure"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_comment_rl_safe incorrectly retried over REST on a non-rate-limit failure"
fi

# --- forge_gh_reopen_issue_rl_safe ---
rc=0
_run_stubbed ratelimited forge_gh_reopen_issue_rl_safe "owner/repo" "99" >/dev/null 2>&1 || rc=$?
assert_eq "0" "$rc" "forge_gh_reopen_issue_rl_safe: rate-limited primary call recovers via REST fallback"
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "^api repos/owner/repo/issues/99 -X PATCH -f state=open" "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_reopen_issue_rl_safe REST fallback PATCHes state=open on the correct issue"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_reopen_issue_rl_safe REST fallback call wrong or missing"
fi

rc=0
_run_stubbed other-error forge_gh_reopen_issue_rl_safe "owner/repo" "99" >/dev/null 2>&1 || rc=$?
assert_eq "1" "$rc" "forge_gh_reopen_issue_rl_safe: non-rate-limit failure propagates"

# --- forge_gh_swap_label_rl_safe ---
rc=0
_run_stubbed ratelimited forge_gh_swap_label_rl_safe "owner/repo" "7" "loom:building" "loom:issue" >/dev/null 2>&1 || rc=$?
assert_eq "0" "$rc" "forge_gh_swap_label_rl_safe: rate-limited primary call recovers via REST fallback"
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "^api repos/owner/repo/issues/7/labels/loom%3Abuilding -X DELETE" "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_swap_label_rl_safe REST fallback DELETEs the percent-encoded old label"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_swap_label_rl_safe REST DELETE call wrong or missing (':' must be %3A-encoded)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q "^api repos/owner/repo/issues/7/labels -f labels\[\]=loom:issue" "$ARGV_LOG"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: forge_gh_swap_label_rl_safe REST fallback POSTs the new label"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: forge_gh_swap_label_rl_safe REST POST call wrong or missing"
fi

rc=0
_run_stubbed other-error forge_gh_swap_label_rl_safe "owner/repo" "7" "loom:building" "loom:issue" >/dev/null 2>&1 || rc=$?
assert_eq "1" "$rc" "forge_gh_swap_label_rl_safe: non-rate-limit failure propagates"

rm -rf "$STUB_DIR"

# --- 3. merge-pr.sh source wiring: the four #4856 call sites use the wrappers ---
echo ""
echo "Testing merge-pr.sh source wiring (#4856)..."

TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'forge_gh_reopen_issue_rl_safe "\$REPO_NWO" "\$issue_num"' "$MERGE_PR_SRC"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: merge-pr.sh's premature-auto-close reopen uses forge_gh_reopen_issue_rl_safe"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: merge-pr.sh missing forge_gh_reopen_issue_rl_safe at the reopen call site"
fi

TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'forge_gh_swap_label_rl_safe "\$REPO_NWO" "\$issue_num" "loom:building" "loom:issue"' "$MERGE_PR_SRC"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: merge-pr.sh's partial-increment label reset uses forge_gh_swap_label_rl_safe"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: merge-pr.sh missing forge_gh_swap_label_rl_safe at the label-swap call site"
fi

comment_call_count="$(grep -c 'forge_gh_comment_rl_safe "\$REPO_NWO"' "$MERGE_PR_SRC" || true)"
assert_eq "3" "$comment_call_count" "merge-pr.sh routes all 3 comment call sites (2x issue, 1x PR) through forge_gh_comment_rl_safe"

# The raw, un-wrapped mutating calls this issue is about must no longer
# appear standalone (they are now routed through the wrapper functions
# above). This deliberately does NOT check `gh issue edit --add-label
# loom:reviewing`-style claim calls elsewhere in the file -- only the four
# specific #4856 call sites.
TESTS_RUN=$((TESTS_RUN + 1))
if ! grep -q 'gh issue reopen "\$issue_num" --repo "\$REPO_NWO" >/dev/null 2>&1' "$MERGE_PR_SRC"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: the old unwrapped 'gh issue reopen' call site is gone"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: the old unwrapped 'gh issue reopen' call site is still present"
fi

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
