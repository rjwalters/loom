#!/usr/bin/env bash
# Test suite for scripts/install/forge-detect.sh
#
# Usage: ./tests/install/test-forge-detect.sh
#
# Tests forge detection logic and URL parsing. Mocks network calls
# so no real API access is needed.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0
TOTAL=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

assert_eq() {
  local desc="$1"
  local expected="$2"
  local actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected: '$expected'"
    echo "  actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

# Source forge-detect (we'll test its helper functions directly)
source "$REPO_ROOT/scripts/install/forge-detect.sh"

# ============================================================================
# Test _extract_host
# ============================================================================
echo ""
echo "=== Testing _extract_host ==="

assert_eq "HTTPS github.com" \
  "github.com" \
  "$(_extract_host "https://github.com/owner/repo.git")"

assert_eq "SSH github.com" \
  "github.com" \
  "$(_extract_host "git@github.com:owner/repo.git")"

assert_eq "HTTPS gitea self-hosted" \
  "gitea.example.com" \
  "$(_extract_host "https://gitea.example.com/owner/repo.git")"

assert_eq "SSH gitea self-hosted" \
  "gitea.example.com" \
  "$(_extract_host "git@gitea.example.com:owner/repo.git")"

assert_eq "HTTPS with port" \
  "gitea.example.com:3000" \
  "$(_extract_host "https://gitea.example.com:3000/owner/repo.git")"

assert_eq "SSH protocol URL" \
  "gitea.example.com" \
  "$(_extract_host "ssh://git@gitea.example.com/owner/repo.git")"

assert_eq "HTTPS no .git suffix" \
  "github.com" \
  "$(_extract_host "https://github.com/owner/repo")"

# ============================================================================
# Test detect_forge_and_repo - GitHub URLs
# ============================================================================
echo ""
echo "=== Testing detect_forge_and_repo (GitHub) ==="

detect_forge_and_repo "https://github.com/rjwalters/loom.git" 2>/dev/null
assert_eq "GitHub HTTPS - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub HTTPS - owner" "rjwalters" "$FORGE_OWNER"
assert_eq "GitHub HTTPS - repo" "loom" "$FORGE_REPO"

detect_forge_and_repo "git@github.com:rjwalters/loom.git" 2>/dev/null
assert_eq "GitHub SSH - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub SSH - owner" "rjwalters" "$FORGE_OWNER"
assert_eq "GitHub SSH - repo" "loom" "$FORGE_REPO"

detect_forge_and_repo "https://github.com/owner/repo" 2>/dev/null
assert_eq "GitHub HTTPS no .git - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub HTTPS no .git - owner" "owner" "$FORGE_OWNER"
assert_eq "GitHub HTTPS no .git - repo" "repo" "$FORGE_REPO"

# ============================================================================
# Test detect_forge_and_repo - error cases
# ============================================================================
echo ""
echo "=== Testing detect_forge_and_repo (error cases) ==="

TOTAL=$((TOTAL + 1))
if detect_forge_and_repo "" 2>/dev/null; then
  echo -e "${RED}FAIL${NC}: Empty URL should fail"
  FAIL=$((FAIL + 1))
else
  echo -e "${GREEN}PASS${NC}: Empty URL returns error"
  PASS=$((PASS + 1))
fi

# Non-GitHub URL with unreachable host should fail
TOTAL=$((TOTAL + 1))
if detect_forge_and_repo "https://unreachable.invalid/owner/repo.git" 2>/dev/null; then
  echo -e "${RED}FAIL${NC}: Unreachable host should fail"
  FAIL=$((FAIL + 1))
else
  echo -e "${GREEN}PASS${NC}: Unreachable host returns error"
  PASS=$((PASS + 1))
fi

# ============================================================================
# Test gitea_api function structure (just verify it's callable)
# ============================================================================
echo ""
echo "=== Testing gitea_api function ==="

TOTAL=$((TOTAL + 1))
if type gitea_api &>/dev/null; then
  echo -e "${GREEN}PASS${NC}: gitea_api function is defined"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: gitea_api function is not defined"
  FAIL=$((FAIL + 1))
fi

# ============================================================================
# Test detect_merge_method (issue #7844)
#
# The installer's FORCE_AUTO_MERGE path must merge with a strategy the target
# repo actually allows rather than a hardcoded "squash". Both `gh` and
# `gitea_api` are shadowed by shell functions here, so no network is touched.
# ============================================================================
echo ""
echo "=== Testing detect_merge_method (#7844) ==="

# Stubbed `gh`: answers the merge-method probe from $STUB_GH_JSON. Setting
# $STUB_GH_FAIL makes it exit non-zero (network/auth failure).
gh() {
  if [[ "${STUB_GH_FAIL:-}" == "1" ]]; then
    return 1
  fi
  if [[ "$1" == "api" ]]; then
    # Emit the tab-separated triple detect_merge_method's --jq expression
    # produces, sourced from the JSON the test declared.
    printf '%s' "${STUB_GH_JSON:-}" | python3 -c '
import json, sys
data = json.load(sys.stdin)
print("\t".join(
    str(data.get(k, False)).lower()
    for k in ("allow_squash_merge", "allow_merge_commit", "allow_rebase_merge")
))
'
    return 0
  fi
  return 0
}

# Stubbed `gitea_api`: emits $STUB_GITEA_BODY followed by $STUB_GITEA_CODE,
# matching the real helper's `body\nhttp_code` output shape.
gitea_api() {
  printf '%s\n%s\n' "${STUB_GITEA_BODY:-}" "${STUB_GITEA_CODE:-200}"
}

FORGE_OWNER="test-owner"
FORGE_REPO="test-repo"

# --- GitHub ---
FORGE_TYPE="github"
STUB_GH_FAIL=""

STUB_GH_JSON='{"allow_squash_merge":true,"allow_merge_commit":true,"allow_rebase_merge":true}'
assert_eq "GitHub: all allowed prefers squash" "squash" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":true}'
assert_eq "GitHub: squash disabled falls back to merge" "merge" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":true}'
assert_eq "GitHub: rebase-only repo uses rebase" "rebase" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":false}'
assert_eq "GitHub: degenerate all-false fails open to squash" "squash" "$(detect_merge_method)"

STUB_GH_JSON='not json at all'
assert_eq "GitHub: unparseable response fails open to squash" "squash" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":false}'
STUB_GH_FAIL=1
assert_eq "GitHub: probe error fails open to squash" "squash" "$(detect_merge_method)"
STUB_GH_FAIL=""

# --- Gitea (note: the merge-commit flag is `allow_merge_commits`, plural) ---
FORGE_TYPE="gitea"
STUB_GITEA_CODE=200

STUB_GITEA_BODY='{"allow_squash_merge":true,"allow_merge_commits":true,"allow_rebase_merge":true}'
assert_eq "Gitea: all allowed prefers squash" "squash" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":true,"allow_rebase_merge":true}'
assert_eq "Gitea: squash disabled falls back to merge" "merge" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":false,"allow_rebase_merge":true}'
assert_eq "Gitea: rebase-only repo uses rebase" "rebase" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":false,"allow_rebase_merge":false}'
assert_eq "Gitea: degenerate all-false fails open to squash" "squash" "$(detect_merge_method)"

STUB_GITEA_BODY='<html>not json</html>'
assert_eq "Gitea: unparseable response fails open to squash" "squash" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":true,"allow_rebase_merge":false}'
STUB_GITEA_CODE=401
assert_eq "Gitea: non-200 response fails open to squash" "squash" "$(detect_merge_method)"
STUB_GITEA_CODE=200

# Never emit anything but the three known strategies.
STUB_GITEA_BODY='{"allow_squash_merge":"yes","allow_merge_commits":"yes"}'
MERGE_METHOD_OUT="$(detect_merge_method)"
TOTAL=$((TOTAL + 1))
if [[ "$MERGE_METHOD_OUT" == "squash" || "$MERGE_METHOD_OUT" == "merge" || "$MERGE_METHOD_OUT" == "rebase" ]]; then
  echo -e "${GREEN}PASS${NC}: output is always one of squash|merge|rebase"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: unexpected merge method '$MERGE_METHOD_OUT'"
  FAIL=$((FAIL + 1))
fi

unset -f gh gitea_api

# The stubs above emulate what `gh api --jq` returns; this check exercises the
# real jq expression from forge-detect.sh (extracted, so it cannot drift) to
# catch a typo in the field names or the @tsv shape. Skipped when jq is absent
# (the installer treats jq as optional — gh ships its own engine).
TOTAL=$((TOTAL + 1))
if command -v jq >/dev/null 2>&1; then
  JQ_EXPR=""
  IFS= read -r JQ_EXPR < <(grep -o "\[(\.allow_squash_merge.*@tsv" "$REPO_ROOT/scripts/install/forge-detect.sh") || true
  JQ_OUT=$(printf '%s' \
    '{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":false}' \
    | jq -r "$JQ_EXPR" 2>/dev/null || echo "")
  if [[ "$JQ_OUT" == $'false\ttrue\tfalse' ]]; then
    echo -e "${GREEN}PASS${NC}: forge-detect.sh jq expression yields the expected TSV triple"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: forge-detect.sh jq expression produced '$JQ_OUT'"
    FAIL=$((FAIL + 1))
  fi
else
  echo -e "${YELLOW}SKIP${NC}: jq not available — cannot verify the --jq expression"
  PASS=$((PASS + 1))
fi

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
