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
