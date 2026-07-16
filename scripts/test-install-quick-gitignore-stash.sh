#!/usr/bin/env bash
# Regression test for issue #3588: `install.sh --quick` must not strand a user's
# uncommitted .gitignore edit across the uninstall→reinstall stash/pop cycle.
#
# The uninstall→init round-trip rewrites .gitignore non-reversibly (uninstall
# strips Loom patterns mid-block + collapses blanks; init re-appends them at
# EOF). That moves lines relative to HEAD, so a stashed .gitignore hunk no longer
# has a matching 3-way base and `git stash pop` conflicts. The old code silenced
# the pop with `2>/dev/null`, hiding the conflict and stranding the edit.
#
# The fix restores .gitignore to HEAD before popping (so the user's hunk applies
# cleanly) and re-appends the Loom patterns idempotently, and surfaces real
# conflicts with a recovery path instead of hiding them.
#
# Requirements: bash, git, and a built loom-daemon binary (target/release).
#   Skips (exit 0) if the daemon binary is not present, mirroring the installer's
#   own build-on-demand behavior — CI that builds the daemon will exercise it.
#
# Usage: bash scripts/test-install-quick-gitignore-stash.sh

set -uo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

passed=0
failed=0

pass() { echo -e "${GREEN}✓${NC} $1"; passed=$((passed + 1)); }
fail() { echo -e "${RED}✗${NC} $1"; failed=$((failed + 1)); }
warn() { echo -e "${YELLOW}!${NC} $1"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_SCRIPT="$LOOM_ROOT/install.sh"

if [[ ! -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
  warn "loom-daemon binary not built (target/release/loom-daemon missing) — skipping #3588 install.sh --quick test"
  exit 0
fi

TEST_DIR="$(mktemp -d)"
cleanup() { [[ -n "${TEST_DIR:-}" && -d "$TEST_DIR" ]] && rm -rf "$TEST_DIR"; }
trap cleanup EXIT

# Create a target repo with a tracked .gitignore (containing a couple of the
# Loom ephemeral patterns mid-block, as a real committed .gitignore would after
# a prior install), install Loom, and commit so we have a clean baseline.
setup_repo() {
  local w="$1"
  git init -q "$w"
  git -C "$w" config user.email test@example.com
  git -C "$w" config user.name "Test User"
  printf 'node_modules/\n.loom/state.json\n.loom/worktrees/\n*.log\ndist/\n' >"$w/.gitignore"
  echo "hello" >"$w/README.md"
  git -C "$w" add -A
  git -C "$w" commit -qm "init"
  "$INSTALL_SCRIPT" -y --quick "$w" >/dev/null 2>&1
  git -C "$w" add -A
  git -C "$w" commit -qm "loom install" >/dev/null 2>&1
}

echo "======================================"
echo "Issue #3588: install.sh --quick .gitignore stash/pop"
echo "======================================"
echo ""

# --------------------------------------------------------------------------
# Scenario 1 (primary): uncommitted .gitignore edit survives the reinstall.
# --------------------------------------------------------------------------
R1="$TEST_DIR/tracked-gitignore-edit"
setup_repo "$R1"
echo "my-local-dir/" >>"$R1/.gitignore"
"$INSTALL_SCRIPT" -y --quick "$R1" >/dev/null 2>&1

if grep -qxF "my-local-dir/" "$R1/.gitignore"; then
  pass "user .gitignore edit ('my-local-dir/') survives --quick reinstall"
else
  fail "user .gitignore edit stranded after --quick reinstall (#3588)"
fi
if [[ "$(git -C "$R1" stash list | wc -l | tr -d ' ')" == "0" ]]; then
  pass "stash consumed (git stash list empty)"
else
  fail "stash not consumed — user change left in stash (#3588)"
fi
if grep -qxF ".loom/worktrees/" "$R1/.gitignore" && grep -qxF ".loom/logs/" "$R1/.gitignore"; then
  pass "current Loom ephemeral patterns present after reinstall"
else
  fail "Loom ephemeral patterns missing after reinstall"
fi
echo ""

# --------------------------------------------------------------------------
# Scenario 2: idempotency — a second --quick run leaves .gitignore byte-stable.
# --------------------------------------------------------------------------
R2="$TEST_DIR/idempotent"
setup_repo "$R2"
echo "my-local-dir/" >>"$R2/.gitignore"
"$INSTALL_SCRIPT" -y --quick "$R2" >/dev/null 2>&1
cp "$R2/.gitignore" "$TEST_DIR/gi-run1"
"$INSTALL_SCRIPT" -y --quick "$R2" >/dev/null 2>&1
if diff -q "$TEST_DIR/gi-run1" "$R2/.gitignore" >/dev/null; then
  pass ".gitignore byte-stable across a second --quick run"
else
  fail ".gitignore churned on second --quick run (not idempotent)"
fi
if grep -qxF "my-local-dir/" "$R2/.gitignore" && \
   [[ "$(git -C "$R2" stash list | wc -l | tr -d ' ')" == "0" ]]; then
  pass "user edit preserved and no phantom stash after second run"
else
  fail "user edit lost or phantom stash after second run"
fi
echo ""

# --------------------------------------------------------------------------
# Scenario 3: unrelated (non-.gitignore) edit still restores cleanly.
# --------------------------------------------------------------------------
R3="$TEST_DIR/readme-edit"
setup_repo "$R3"
echo "user readme line" >>"$R3/README.md"
"$INSTALL_SCRIPT" -y --quick "$R3" >/dev/null 2>&1
if grep -qxF "user readme line" "$R3/README.md" && \
   [[ "$(git -C "$R3" stash list | wc -l | tr -d ' ')" == "0" ]]; then
  pass "unrelated README.md edit restored cleanly (no regression)"
else
  fail "unrelated README.md edit not restored"
fi
echo ""

# --------------------------------------------------------------------------
# Scenario 4: .gitignore untracked at HEAD — reset is skipped, no error raised.
# --------------------------------------------------------------------------
R4="$TEST_DIR/untracked-gitignore"
git init -q "$R4"
git -C "$R4" config user.email test@example.com
git -C "$R4" config user.name "Test User"
echo "hello" >"$R4/README.md"
git -C "$R4" add README.md
git -C "$R4" commit -qm "init"
"$INSTALL_SCRIPT" -y --quick "$R4" >/dev/null 2>&1
# Keep .gitignore untracked at HEAD.
git -C "$R4" add -A
git -C "$R4" reset -q -- .gitignore
git -C "$R4" commit -qm "loom minus gitignore" >/dev/null 2>&1
echo "another readme line" >>"$R4/README.md"
if "$INSTALL_SCRIPT" -y --quick "$R4" >/dev/null 2>&1 && \
   grep -qxF "another readme line" "$R4/README.md"; then
  pass "untracked-.gitignore reinstall completes cleanly and restores edit"
else
  fail "untracked-.gitignore reinstall failed"
fi
echo ""

echo "======================================"
echo "Test Summary"
echo "======================================"
echo -e "${GREEN}Passed: $passed${NC}"
echo -e "${RED}Failed: $failed${NC}"
echo ""
if [[ $failed -eq 0 ]]; then
  echo -e "${GREEN}All tests passed!${NC}"
  exit 0
else
  echo -e "${RED}Some tests failed.${NC}"
  exit 1
fi
