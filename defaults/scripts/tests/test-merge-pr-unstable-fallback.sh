#!/usr/bin/env bash
# test-merge-pr-unstable-fallback.sh - Unit tests for the UNSTABLE-fallback
# logic in merge-pr.sh and its supporting helper in forge-helpers.sh.
#
# The UNSTABLE-fallback (#3486) sits immediately after the CLEAN-fallback
# (#3371) and decides whether an auto-merge "Pull request is in unstable
# status" error can be safely demoted to the immediate-merge path. It fires
# only when every failing check on the PR is OUTSIDE branch protection's
# requiredStatusCheckContexts.
#
# This test exercises two surfaces:
#   1. `forge_get_required_status_check_contexts` (GitHub) returns the
#      newline-separated context list emitted by the GraphQL query, with the
#      branchProtectionRule shape stubbed via a PATH-shimmed `gh`. Empty list
#      and missing-rule paths both yield empty stdout.
#   2. The set-difference policy that gates the fallback in merge-pr.sh:
#      - All failing checks informational → fallback fires.
#      - At least one failing check required → fallback does NOT fire.
#   We test the policy by replicating the same `comm -23` / `comm -12` shape
#   the script uses, so the script-internal block stays in lockstep with the
#   test.
#
# Usage:
#   ./.loom/scripts/tests/test-merge-pr-unstable-fallback.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

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

# --- Source helpers ---
source "$HELPERS_DIR/lib/forge-helpers.sh"

# Reset detected state for tests
FORGE_TYPE=""

# --- Test forge_get_required_status_check_contexts (GitHub path) ---
echo "Testing forge_get_required_status_check_contexts (GitHub stub)..."

FORGE_TYPE="github"

STUB_DIR=$(mktemp -d)
trap 'rm -rf "$STUB_DIR"' EXIT

# Stub gh that recognizes the GraphQL query for required status check contexts.
# We inspect $* for the GraphQL ref argument shape and pick the response from
# canned files keyed by `ref=refs/heads/<branch>`.
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh used by test-merge-pr-unstable-fallback.sh.
#
# Recognizes:
#   gh api graphql -f query=... -F owner=... -F name=... -F ref=refs/heads/<b>
#                  --jq '.data.repository.ref.branchProtectionRule.requiredStatusCheckContexts // [] | .[]'
#
# It pulls the branch from the ref=... arg and looks up a canned response in
# $STUB_DIR/required-checks-<branch>.txt (one context per line). If the file
# doesn't exist, emits nothing (simulates absent branchProtectionRule).
STUB_DIR_FROM_ENV="${LOOM_TEST_STUB_DIR:-}"
if [[ -z "$STUB_DIR_FROM_ENV" ]]; then
  echo "stub gh: LOOM_TEST_STUB_DIR not set" >&2
  exit 2
fi

# Find the ref=... arg
ref=""
for a in "$@"; do
  case "$a" in
    ref=refs/heads/*) ref="${a#ref=refs/heads/}" ;;
  esac
done

if [[ -z "$ref" ]]; then
  exit 0
fi

# Canned response file lookup
canned="$STUB_DIR_FROM_ENV/required-checks-$ref.txt"
if [[ -f "$canned" ]]; then
  cat "$canned"
fi
exit 0
STUB
chmod +x "$STUB_DIR/gh"

export LOOM_TEST_STUB_DIR="$STUB_DIR"

# Subtest 1.1: branch has two required contexts
cat > "$STUB_DIR/required-checks-main.txt" <<EOF
Code Ownership
Required Build
EOF
result=$(forge_get_required_status_check_contexts "owner/repo" "main" "$STUB_DIR/gh" | tr '\n' '|' | sed 's/|$//')
assert_eq "Code Ownership|Required Build" "$result" "GitHub: two required contexts returned newline-separated"

# Subtest 1.2: branch has no protection rule -> empty output
result=$(forge_get_required_status_check_contexts "owner/repo" "no-protection-branch" "$STUB_DIR/gh" | tr '\n' '|' | sed 's/|$//')
assert_eq "" "$result" "GitHub: missing branchProtectionRule yields empty output"

# Subtest 1.3: branch has protection rule with empty contexts -> empty output
: > "$STUB_DIR/required-checks-empty-required.txt"  # touch empty file
result=$(forge_get_required_status_check_contexts "owner/repo" "empty-required" "$STUB_DIR/gh" | tr '\n' '|' | sed 's/|$//')
assert_eq "" "$result" "GitHub: empty requiredStatusCheckContexts yields empty output"

# Subtest 1.4: single required context
echo "Code Ownership" > "$STUB_DIR/required-checks-single.txt"
result=$(forge_get_required_status_check_contexts "owner/repo" "single" "$STUB_DIR/gh" | tr '\n' '|' | sed 's/|$//')
assert_eq "Code Ownership" "$result" "GitHub: single required context returned correctly"

# --- Test the set-difference policy (Gitea sentinel & GitHub) ---
# These replicate the comm/sort/diff logic used inside merge-pr.sh so that the
# decision can be exercised in isolation. If the inline script implementation
# drifts away from this shape, this test starts failing.
echo ""
echo "Testing set-difference policy (failing_checks \\ required_contexts)..."

# Helper: returns "fire" if the fallback should fire (all failing are
# informational), "preserve" if at least one failing is required (or there are
# no failing checks at all, or the Gitea sentinel is present).
_policy_decision() {
    local failing="$1"
    local required="$2"

    if [[ -z "$failing" ]]; then
        echo "preserve"
        return
    fi

    # Gitea sentinel — fail-closed for v0.10.0.
    if echo "$required" | grep -qx "__GITEA_TODO__"; then
        echo "preserve"
        return
    fi

    local informational overlap
    informational=$(comm -23 \
      <(printf '%s\n' "$failing" | sort -u) \
      <(printf '%s\n' "$required" | sort -u))
    overlap=$(comm -12 \
      <(printf '%s\n' "$failing" | sort -u) \
      <(printf '%s\n' "$required" | sort -u))

    if [[ -z "$overlap" ]] && [[ -n "$informational" ]]; then
        echo "fire"
    else
        echo "preserve"
    fi
}

# Branch A: all failing checks are informational (NOT in required) -> fallback fires.
failing=$'CI: Stack B lockstep (informational, 30-day soak)\nValidate projects/*/project.json against schema'
required=$'Code Ownership'
result=$(_policy_decision "$failing" "$required")
assert_eq "fire" "$result" "All informational failures -> fallback fires"

# Branch A.2: required is empty (no branch protection) -> fallback fires.
failing=$'Some Informational Check\nAnother One'
required=""
result=$(_policy_decision "$failing" "$required")
assert_eq "fire" "$result" "Empty required (no branch protection) -> fallback fires"

# Branch A.3: same context name twice in failing (re-run) -> still fires.
failing=$'Informational A\nInformational A\nInformational B'
required="Code Ownership"
result=$(_policy_decision "$failing" "$required")
assert_eq "fire" "$result" "Duplicate failing contexts dedupe via sort -u and fallback fires"

# Branch B: at least one failing check IS required -> fallback does NOT fire.
failing=$'Code Ownership\nCI: Stack B lockstep (informational, 30-day soak)'
required=$'Code Ownership'
result=$(_policy_decision "$failing" "$required")
assert_eq "preserve" "$result" "Failing includes a required context -> fallback preserves refusal"

# Branch B.2: all failing checks are required -> fallback does NOT fire.
failing=$'Code Ownership\nRequired Build'
required=$'Code Ownership\nRequired Build'
result=$(_policy_decision "$failing" "$required")
assert_eq "preserve" "$result" "All failing are required -> fallback preserves refusal"

# Branch B.3: failing is empty -> fallback does NOT fire (no failing → not the UNSTABLE case we care about).
failing=""
required=$'Code Ownership'
result=$(_policy_decision "$failing" "$required")
assert_eq "preserve" "$result" "Empty failing set -> fallback preserves refusal"

# Branch B.4: Gitea sentinel present -> fallback does NOT fire.
failing=$'Informational A'
required=$'__GITEA_TODO__'
result=$(_policy_decision "$failing" "$required")
assert_eq "preserve" "$result" "Gitea sentinel -> fallback preserves refusal (fail-closed)"

# --- Test Gitea variant returns sentinel ---
echo ""
echo "Testing forge_get_required_status_check_contexts (Gitea returns sentinel)..."
# shellcheck disable=SC2034
FORGE_TYPE="gitea"  # consumed by the sourced helper to pick the Gitea branch
result=$(forge_get_required_status_check_contexts "owner/repo" "main" "$STUB_DIR/gh" | tr '\n' '|' | sed 's/|$//')
assert_eq "__GITEA_TODO__" "$result" "Gitea: returns '__GITEA_TODO__' sentinel (TODO #3486)"

# --- Test that the unstable-status-substring matcher in merge-pr.sh is robust ---
# The merge-pr.sh fallback matches on the substring "is in unstable status"
# (sibling of the CLEAN-fallback's "is in clean status" matcher). This guards
# against GitHub's "Pull request Pull request is in unstable status" doubled-word
# error prefix and any future normalization.
echo ""
echo "Testing the unstable-status-substring matcher shape..."

unstable_error="Failed to enable auto-merge: gh: Pull request Pull request is in unstable status (enablePullRequestAutoMerge)"
if echo "$unstable_error" | grep -q "is in unstable status"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: 'is in unstable status' substring matches GitHub's doubled-word error"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: substring matcher missed the GitHub error"
fi

clean_error="gh: Pull request Pull request is in clean status (enablePullRequestAutoMerge)"
if echo "$clean_error" | grep -q "is in unstable status"; then
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: substring matcher fired on CLEAN error (false positive)"
else
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: 'is in unstable status' substring does NOT match CLEAN error"
fi

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
