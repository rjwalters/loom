#!/usr/bin/env bash
# Shared assertion helpers for the tests/install suites.
#
# Source this near the top of a suite, after SCRIPT_DIR is set:
#   source "$SCRIPT_DIR/lib/assert.sh"
#
# Provides RED/GREEN/NC colour vars, the PASS/FAIL/TOTAL counters, the
# desc-first assert_eq (desc expected actual), and make_fake_github_repo.
# (defaults/scripts/tests/lib/assert.sh is the sibling lib for the other
# suites; it uses a different argument order, so it cannot be sourced here.)

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0
TOTAL=0

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
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

# Create a temp git repo whose origin is a github.com URL; prints its path.
make_fake_github_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "$dir" init -q
  git -C "$dir" remote add origin "https://github.com/owner/repo.git"
  printf '%s\n' "$dir"
}
