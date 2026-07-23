#!/usr/bin/env bash
# Test suite for install-loom.sh --local / --gitignore mode (issue #3836).
#
# Covers both the sourceable helper (scripts/install/local-mode.sh) in isolation
# and the installer's --local short-circuit end-to-end. Neither path builds the
# daemon or touches the network — the short-circuit runs before every heavy step
# — so this suite runs fast against local temp git repos with no gh auth.
#
# Usage: bash scripts/test-install-local-mode.sh

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
INSTALL_SCRIPT="$LOOM_ROOT/scripts/install-loom.sh"
HELPER="$LOOM_ROOT/scripts/install/local-mode.sh"

TEST_DIR="$(mktemp -d)"
cleanup() { [[ -n "${TEST_DIR:-}" && -d "$TEST_DIR" ]] && rm -rf "$TEST_DIR"; }
trap cleanup EXIT

# Create a fresh git repo that already has Loom implementation files committed
# (simulating a prior committed install), plus a project-specific labels.yml.
make_repo() {
  local repo="$1"
  mkdir -p "$repo"
  git -C "$repo" init --quiet
  git -C "$repo" config user.email "test@example.com"
  git -C "$repo" config user.name "Test"
  mkdir -p "$repo/.loom/scripts" \
           "$repo/.claude/commands/loom" \
           "$repo/.claude/agents" \
           "$repo/.github"
  echo '{}' > "$repo/.loom/config.json"
  echo 'echo hi' > "$repo/.loom/scripts/worktree.sh"
  echo 'builder cmd' > "$repo/.claude/commands/loom/builder.md"
  echo 'builder agent' > "$repo/.claude/agents/loom-builder.md"
  echo 'not loom' > "$repo/.claude/agents/other.md"
  echo 'labels' > "$repo/.github/labels.yml"
  echo 'node_modules/' > "$repo/.gitignore"
  git -C "$repo" add -A
  git -C "$repo" commit -q -m "initial with committed loom files"
}

echo "======================================"
echo "install-loom.sh --local mode tests"
echo "======================================"
echo ""

# ==========================================================================
# Helper-level tests (source local-mode.sh directly)
# ==========================================================================
echo "--- Helper (scripts/install/local-mode.sh) ---"

HREPO="$TEST_DIR/helper-repo"
make_repo "$HREPO"

(
  set +e
  # shellcheck source=scripts/install/local-mode.sh
  source "$HELPER"

  # 1. apply_gitignore writes the managed block
  loom_local_apply_gitignore "$HREPO"
  gi="$HREPO/.gitignore"
  if grep -qF "$LOOM_LOCAL_GITIGNORE_BEGIN" "$gi" \
     && grep -qF "$LOOM_LOCAL_GITIGNORE_END" "$gi" \
     && grep -qF "/.loom/" "$gi" \
     && grep -qF "/.claude/commands/loom/" "$gi" \
     && grep -qF "/.claude/agents/loom-*.md" "$gi" \
     && grep -qF "/.loom-local/" "$gi"; then
    echo "PASS block-written"
  else
    echo "FAIL block-written"
  fi

  # Pre-existing content preserved
  grep -qF "node_modules/" "$gi" && echo "PASS preexisting-preserved" || echo "FAIL preexisting-preserved"

  # 2. Idempotency — re-run produces exactly one block, identical file
  before="$(cat "$gi")"
  loom_local_apply_gitignore "$HREPO"
  loom_local_apply_gitignore "$HREPO"
  after="$(cat "$gi")"
  begin_count="$(grep -cF "$LOOM_LOCAL_GITIGNORE_BEGIN" "$gi")"
  if [[ "$before" == "$after" ]] && [[ "$begin_count" -eq 1 ]]; then
    echo "PASS idempotent"
  else
    echo "FAIL idempotent (begin_count=$begin_count)"
  fi

  # 3. tracked-paths detection finds the committed impl paths (and the glob)
  tracked="$(loom_local_tracked_paths "$HREPO")"
  if echo "$tracked" | grep -qxF ".loom" \
     && echo "$tracked" | grep -qxF ".claude/commands/loom" \
     && echo "$tracked" | grep -qxF ".claude/agents/loom-*.md"; then
    echo "PASS tracked-detected"
  else
    echo "FAIL tracked-detected [$tracked]"
  fi

  # 4. untrack-commands prints quoted git rm lines
  cmds="$(loom_local_untrack_commands "$HREPO")"
  if echo "$cmds" | grep -qF "git rm -r --cached -- '.loom'" \
     && echo "$cmds" | grep -qF "git rm -r --cached -- '.claude/agents/loom-*.md'"; then
    echo "PASS untrack-commands"
  else
    echo "FAIL untrack-commands"
  fi

  # 5. run_untrack actually removes them from the index (files stay on disk)
  loom_local_run_untrack "$HREPO"
  still="$(loom_local_tracked_paths "$HREPO")"
  if [[ -z "$still" ]] \
     && [[ -f "$HREPO/.loom/config.json" ]] \
     && [[ -f "$HREPO/.claude/agents/loom-builder.md" ]]; then
    echo "PASS run-untrack"
  else
    echo "FAIL run-untrack [$still]"
  fi

  # 6. non-loom + project config stay tracked
  if [[ -n "$(git -C "$HREPO" ls-files -- .github/labels.yml)" ]] \
     && [[ -n "$(git -C "$HREPO" ls-files -- .claude/agents/other.md)" ]]; then
    echo "PASS project-config-preserved"
  else
    echo "FAIL project-config-preserved"
  fi
) > "$TEST_DIR/helper.out" 2>&1

while IFS= read -r line; do
  case "$line" in
    PASS*) pass "${line#PASS }" ;;
    FAIL*) fail "${line#FAIL }" ;;
  esac
done < "$TEST_DIR/helper.out"
echo ""

# ==========================================================================
# End-to-end: install-loom.sh --local
# ==========================================================================
echo "--- install-loom.sh --local (end-to-end) ---"

# 1. --local without --untrack: writes block, prints commands, leaves tracked.
E1="$TEST_DIR/e2e-print"
make_repo "$E1"
OUT1="$(bash "$INSTALL_SCRIPT" --local "$E1" 2>&1)" && rc1=0 || rc1=$?
if [[ "$rc1" -eq 0 ]]; then pass "--local exits 0"; else fail "--local exits 0 (rc=$rc1)"; fi
if grep -qF "# >>> loom-local install (do not edit) >>>" "$E1/.gitignore"; then
  pass "--local wrote gitignore block"
else
  fail "--local wrote gitignore block"
fi
if echo "$OUT1" | grep -qF "git rm -r --cached -- '.loom'"; then
  pass "--local printed untrack commands"
else
  fail "--local printed untrack commands"
fi
if [[ -n "$(git -C "$E1" ls-files -- .loom)" ]]; then
  pass "--local (no --untrack) leaves files tracked"
else
  fail "--local (no --untrack) leaves files tracked"
fi

# 2. --local --untrack: files no longer tracked afterward (core acceptance).
E2="$TEST_DIR/e2e-untrack"
make_repo "$E2"
bash "$INSTALL_SCRIPT" --local --untrack "$E2" > "$TEST_DIR/e2e2.out" 2>&1 && rc2=0 || rc2=$?
if [[ "$rc2" -eq 0 ]]; then pass "--local --untrack exits 0"; else fail "--local --untrack exits 0 (rc=$rc2)"; fi
if [[ -z "$(git -C "$E2" ls-files -- .loom)" ]] \
   && [[ -z "$(git -C "$E2" ls-files -- .claude/commands/loom)" ]] \
   && [[ -z "$(git -C "$E2" ls-files -- '.claude/agents/loom-*.md')" ]]; then
  pass "--local --untrack: Loom impl files not tracked afterward"
else
  fail "--local --untrack: Loom impl files not tracked afterward"
fi
if [[ -f "$E2/.loom/config.json" ]] && [[ -n "$(git -C "$E2" ls-files -- .github/labels.yml)" ]]; then
  pass "--local --untrack: files on disk + project config still tracked"
else
  fail "--local --untrack: files on disk + project config still tracked"
fi

# 3. --gitignore alias behaves like --local
E3="$TEST_DIR/e2e-alias"
make_repo "$E3"
bash "$INSTALL_SCRIPT" --gitignore "$E3" > /dev/null 2>&1 || true
if grep -qF "# >>> loom-local install (do not edit) >>>" "$E3/.gitignore"; then
  pass "--gitignore alias writes block"
else
  fail "--gitignore alias writes block"
fi

# 4. Idempotency end-to-end: running twice keeps a single block.
bash "$INSTALL_SCRIPT" --gitignore "$E3" > /dev/null 2>&1 || true
if [[ "$(grep -cF "# >>> loom-local install (do not edit) >>>" "$E3/.gitignore")" -eq 1 ]]; then
  pass "--local end-to-end idempotent (single block)"
else
  fail "--local end-to-end idempotent (single block)"
fi

# 5. --untrack without --local is rejected.
E4="$TEST_DIR/e2e-guard"
make_repo "$E4"
if bash "$INSTALL_SCRIPT" --untrack "$E4" > /dev/null 2>&1; then
  fail "--untrack without --local should error"
else
  pass "--untrack without --local errors"
fi

# 6. --local on a repo with no prior .gitignore creates one with the block.
E5="$TEST_DIR/e2e-nogitignore"
mkdir -p "$E5"
git -C "$E5" init --quiet
git -C "$E5" config user.email "t@e.com"; git -C "$E5" config user.name "T"
echo x > "$E5/f"; git -C "$E5" add -A; git -C "$E5" commit -q -m init
bash "$INSTALL_SCRIPT" --local "$E5" > /dev/null 2>&1 || true
if [[ -f "$E5/.gitignore" ]] && grep -qF "/.loom/" "$E5/.gitignore"; then
  pass "--local creates .gitignore when absent"
else
  fail "--local creates .gitignore when absent"
fi

# 7. --local help text is documented.
HELP_OUT="$(bash "$INSTALL_SCRIPT" --help 2>&1)"
if printf '%s' "$HELP_OUT" | grep -qF -- "--local"; then
  pass "--local documented in --help"
else
  fail "--local documented in --help"
fi

echo ""
echo "======================================"
echo "Test Summary"
echo "======================================"
echo -e "${GREEN}Passed: $passed${NC}"
echo -e "${RED}Failed: $failed${NC}"
echo ""

if [ "$failed" -eq 0 ]; then
  echo -e "${GREEN}All tests passed!${NC}"
  exit 0
else
  echo -e "${RED}Some tests failed.${NC}"
  exit 1
fi
