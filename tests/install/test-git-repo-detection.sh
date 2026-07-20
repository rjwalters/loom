#!/usr/bin/env bash
# Test suite for git-repository detection in the installer/uninstaller.
#
# Usage: ./tests/install/test-git-repo-detection.sh
#
# Regression guard for #3649: a linked git worktree's `.git` is a FILE
# (a `gitdir:` pointer), so the old `[[ -d "$TARGET_PATH/.git" ]]` idiom
# misdetected a worktree as a non-repo. The canonical worktree-safe check
# is `git -C "$dir" rev-parse --git-dir`, which succeeds for both normal
# repos and linked worktrees. This test exercises that expression directly
# against three fixtures.
#
# Self-contained, no network. Exit code 0 = all tests pass, 1 = failures.

set -euo pipefail

PASS=0
FAIL=0
TOTAL=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
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

# The detection expression under test — mirrors the fixed call sites in
# install.sh, scripts/install/validate-target.sh, scripts/uninstall-loom.sh.
# Echoes "repo" when the directory is a git repo (or worktree), "notrepo" otherwise.
detect() {
  local dir="$1"
  if git -C "$dir" rev-parse --git-dir >/dev/null 2>&1; then
    echo "repo"
  else
    echo "notrepo"
  fi
}

# ----------------------------------------------------------------------------
# Fixtures
# ----------------------------------------------------------------------------
WORK_DIR="$(mktemp -d)"
cleanup() {
  # `git worktree add` may leave the linked worktree registered; rm -rf is
  # sufficient for temp fixtures since the parent repo is also disposable.
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

# Isolate git config so host user settings / hooks don't interfere.
export GIT_CONFIG_GLOBAL="$WORK_DIR/gitconfig"
export GIT_CONFIG_SYSTEM=/dev/null
export HOME="$WORK_DIR"
git config --global user.email "test@example.com"
git config --global user.name "Test"
git config --global init.defaultBranch main

# Fixture 1: a normal git repository.
NORMAL_REPO="$WORK_DIR/normal-repo"
mkdir -p "$NORMAL_REPO"
git -C "$NORMAL_REPO" init -q
git -C "$NORMAL_REPO" commit -q --allow-empty -m "initial"

# Fixture 2: a linked worktree (its `.git` is a FILE, not a directory).
LINKED_WORKTREE="$WORK_DIR/linked-worktree"
git -C "$NORMAL_REPO" worktree add -q -b feature-branch "$LINKED_WORKTREE"

# Fixture 3: an empty, non-repo directory.
NON_REPO="$WORK_DIR/non-repo"
mkdir -p "$NON_REPO"

# ----------------------------------------------------------------------------
# Sanity: confirm the fixture shapes are what we expect.
# ----------------------------------------------------------------------------
echo ""
echo "=== Fixture sanity checks ==="

TOTAL=$((TOTAL + 1))
if [[ -d "$NORMAL_REPO/.git" ]]; then
  echo -e "${GREEN}PASS${NC}: normal repo .git is a directory"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: normal repo .git is a directory"
  FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
if [[ -f "$LINKED_WORKTREE/.git" ]]; then
  echo -e "${GREEN}PASS${NC}: linked worktree .git is a FILE (the regression trigger)"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: linked worktree .git is a FILE (the regression trigger)"
  FAIL=$((FAIL + 1))
fi

# ----------------------------------------------------------------------------
# Detection behavior
# ----------------------------------------------------------------------------
echo ""
echo "=== Testing git-repo detection ==="

assert_eq "normal git repo is detected as a repo" \
  "repo" \
  "$(detect "$NORMAL_REPO")"

assert_eq "linked worktree is detected as a repo (regression #3649)" \
  "repo" \
  "$(detect "$LINKED_WORKTREE")"

assert_eq "empty non-repo dir is correctly flagged as not a repo" \
  "notrepo" \
  "$(detect "$NON_REPO")"

# ----------------------------------------------------------------------------
# Regression guard: the OLD broken idiom would misdetect the worktree.
# This documents WHY the fix was needed; it asserts the old check was wrong.
# ----------------------------------------------------------------------------
echo ""
echo "=== Regression guard: old idiom would misdetect the worktree ==="

old_detect() {
  local dir="$1"
  if [[ -d "$dir/.git" ]]; then echo "repo"; else echo "notrepo"; fi
}

assert_eq "old '-d .git' idiom misdetects the worktree (documents the bug)" \
  "notrepo" \
  "$(old_detect "$LINKED_WORKTREE")"

# ----------------------------------------------------------------------------
# Summary
# ----------------------------------------------------------------------------
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
