#!/usr/bin/env bash
# Integration test suite for install-loom.sh and uninstall-loom.sh
#
# Tests the installer and uninstaller scripts against temporary Git repositories.
# Follows the test-daemon-scripts.sh pattern (pass/fail counters, colored output).
#
# Requirements:
#   - bash, git (standard on all platforms)
#   - Tests run against local temp repos — no gh CLI authentication needed
#
# Usage:
#   bash scripts/test-installer.sh

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

passed=0
failed=0

# Helper functions
pass() {
  echo -e "${GREEN}✓${NC} $1"
  passed=$((passed + 1))
}

fail() {
  echo -e "${RED}✗${NC} $1"
  failed=$((failed + 1))
}

warn() {
  echo -e "${YELLOW}!${NC} $1"
}

# Determine paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_SCRIPT="$LOOM_ROOT/scripts/install-loom.sh"
UNINSTALL_SCRIPT="$LOOM_ROOT/scripts/uninstall-loom.sh"
DEFAULTS_DIR="$LOOM_ROOT/defaults"

# Temp directory for all test repos
TEST_DIR=""

cleanup() {
  if [[ -n "${TEST_DIR:-}" ]] && [[ -d "$TEST_DIR" ]]; then
    rm -rf "$TEST_DIR"
  fi
}

trap cleanup EXIT

# Create the shared temp directory
TEST_DIR=$(mktemp -d)

# Create a bare temp git repo with an initial commit
create_temp_repo() {
  local repo_dir="$1"
  mkdir -p "$repo_dir"
  git -C "$repo_dir" init --quiet
  git -C "$repo_dir" config user.email "test@test.com"
  git -C "$repo_dir" config user.name "Test"
  git -C "$repo_dir" commit --allow-empty -m "Initial commit" --quiet
}

# Simulate a Loom installation by copying defaults into a target repo.
# This mimics what install-loom.sh + loom-daemon init produce, without
# needing the loom-daemon binary or GitHub authentication.
simulate_loom_install() {
  local target="$1"

  # Create .loom directory structure
  mkdir -p "$target/.loom/roles"
  mkdir -p "$target/.loom/scripts"
  mkdir -p "$target/.loom/hooks"
  mkdir -p "$target/.loom/bin"
  mkdir -p "$target/.loom/docs"

  # Copy role definitions
  if [[ -d "$DEFAULTS_DIR/roles" ]]; then
    cp "$DEFAULTS_DIR/roles/"*.md "$target/.loom/roles/" 2>/dev/null || true
    cp "$DEFAULTS_DIR/roles/"*.json "$target/.loom/roles/" 2>/dev/null || true
  fi

  # Copy scripts
  if [[ -d "$DEFAULTS_DIR/scripts" ]]; then
    cp -r "$DEFAULTS_DIR/scripts/"* "$target/.loom/scripts/" 2>/dev/null || true
  fi

  # Copy hooks
  if [[ -d "$DEFAULTS_DIR/hooks" ]]; then
    for hook in "$DEFAULTS_DIR/hooks/"*.sh; do
      [[ -f "$hook" ]] || continue
      cp "$hook" "$target/.loom/hooks/"
      chmod +x "$target/.loom/hooks/$(basename "$hook")"
    done
  fi

  # Copy config
  if [[ -f "$DEFAULTS_DIR/config.json" ]]; then
    cp "$DEFAULTS_DIR/config.json" "$target/.loom/config.json"
  fi

  # Copy CLI wrapper
  if [[ -d "$DEFAULTS_DIR/.loom/bin" ]]; then
    cp "$DEFAULTS_DIR/.loom/bin/"* "$target/.loom/bin/" 2>/dev/null || true
    chmod +x "$target/.loom/bin/"* 2>/dev/null || true
  fi

  # Copy CLAUDE.md
  if [[ -f "$DEFAULTS_DIR/CLAUDE.md" ]]; then
    cp "$DEFAULTS_DIR/CLAUDE.md" "$target/CLAUDE.md"
  fi

  # Copy .claude directory
  if [[ -d "$DEFAULTS_DIR/.claude" ]]; then
    cp -r "$DEFAULTS_DIR/.claude" "$target/.claude"
  fi

  # Copy .codex directory
  if [[ -d "$DEFAULTS_DIR/.codex" ]]; then
    cp -r "$DEFAULTS_DIR/.codex" "$target/.codex"
  fi

  # Copy .github directory (labels.yml)
  if [[ -d "$DEFAULTS_DIR/.github" ]]; then
    mkdir -p "$target/.github"
    cp -r "$DEFAULTS_DIR/.github/"* "$target/.github/"
  fi

  # Create .gitignore with Loom runtime patterns (as loom-daemon init would)
  cat >> "$target/.gitignore" << 'GITIGNORE_EOF'
# Loom - AI Development Orchestration
.loom/state.json
.loom/worktrees/
.loom/*.log
.loom/*.sock
.loom/config.json
.loom/daemon-state.json
.loom/progress/
.loom/loom-source-path
.loom/install-metadata.json
.loom/manifest.json
.loom/.daemon.*
GITIGNORE_EOF

  # Create install metadata
  cat > "$target/.loom/install-metadata.json" << 'META_EOF'
{
  "loom_version": "0.0.0-test",
  "loom_commit": "test",
  "install_date": "2026-01-01",
  "loom_source": "/tmp/test-loom"
}
META_EOF

  # Create loom-source-path
  echo "$LOOM_ROOT" > "$target/.loom/loom-source-path"

  # Commit the installed state
  git -C "$target" add -A
  git -C "$target" commit -m "Install Loom" --quiet
}


echo "======================================"
echo "Installer/Uninstaller Test Suite"
echo "======================================"
echo ""


# ==========================================================================
# Section 1: Argument Validation
# ==========================================================================
echo "--- Section 1: Argument Validation ---"
echo ""

# Test 1: install --help
echo "Test 1: install-loom.sh --help exits 0"
if "$INSTALL_SCRIPT" --help > /dev/null 2>&1; then
  pass "install --help exits successfully"
else
  fail "install --help exited with error"
fi
echo ""

# Test 2: install without path
echo "Test 2: install-loom.sh without path exits with error"
if "$INSTALL_SCRIPT" --yes 2>/dev/null; then
  fail "install without path should have failed"
else
  pass "install without path exits with error"
fi
echo ""

# Test 3: install with non-existent path
echo "Test 3: install-loom.sh rejects non-existent path"
if "$INSTALL_SCRIPT" --yes "/tmp/nonexistent-path-$$-$(date +%s)" 2>/dev/null; then
  fail "install with non-existent path should have failed"
else
  pass "install rejects non-existent path"
fi
echo ""

# Test 4: install rejects non-git directory
echo "Test 4: install-loom.sh rejects non-git directory"
NON_GIT_DIR="$TEST_DIR/not-a-repo"
mkdir -p "$NON_GIT_DIR"
if "$INSTALL_SCRIPT" --yes "$NON_GIT_DIR" 2>/dev/null; then
  fail "install should reject non-git directory"
else
  pass "install rejects non-git directory"
fi
echo ""

# Test 5: uninstall --help
echo "Test 5: uninstall-loom.sh --help exits 0"
if "$UNINSTALL_SCRIPT" --help > /dev/null 2>&1; then
  pass "uninstall --help exits successfully"
else
  fail "uninstall --help exited with error"
fi
echo ""

# Test 6: uninstall without path
echo "Test 6: uninstall-loom.sh without path exits with error"
if "$UNINSTALL_SCRIPT" --yes 2>/dev/null; then
  fail "uninstall without path should have failed"
else
  pass "uninstall without path exits with error"
fi
echo ""

# Test 7: uninstall rejects repo without Loom installed
echo "Test 7: uninstall-loom.sh rejects repo without Loom"
EMPTY_REPO="$TEST_DIR/empty-repo"
create_temp_repo "$EMPTY_REPO"
if "$UNINSTALL_SCRIPT" --yes --local "$EMPTY_REPO" 2>/dev/null; then
  fail "uninstall should reject repo without Loom"
else
  pass "uninstall rejects repo without Loom installed"
fi
echo ""

# Test 8: uninstall rejects Loom source repository
echo "Test 8: uninstall-loom.sh rejects Loom source repo"
if "$UNINSTALL_SCRIPT" --yes --local "$LOOM_ROOT" 2>/dev/null; then
  fail "uninstall should reject Loom source repository"
else
  pass "uninstall rejects Loom source repository"
fi
echo ""


# ==========================================================================
# Section 2: Simulated Install State Verification
# ==========================================================================
echo "--- Section 2: Install State Verification ---"
echo ""

INSTALL_REPO="$TEST_DIR/install-test"
create_temp_repo "$INSTALL_REPO"
simulate_loom_install "$INSTALL_REPO"

# Test 9: .loom directory
echo "Test 9: Install creates .loom directory"
if [[ -d "$INSTALL_REPO/.loom" ]]; then
  pass ".loom directory exists"
else
  fail ".loom directory missing"
fi

# Test 10: CLAUDE.md
echo "Test 10: Install creates CLAUDE.md"
if [[ -f "$INSTALL_REPO/CLAUDE.md" ]]; then
  pass "CLAUDE.md exists"
else
  fail "CLAUDE.md missing"
fi

# Test 11: .claude/commands
echo "Test 11: Install creates .claude/commands"
if [[ -d "$INSTALL_REPO/.claude/commands" ]]; then
  pass ".claude/commands directory exists"
else
  fail ".claude/commands directory missing"
fi

# Test 12: .claude/settings.json
echo "Test 12: Install creates .claude/settings.json"
if [[ -f "$INSTALL_REPO/.claude/settings.json" ]]; then
  pass ".claude/settings.json exists"
else
  fail ".claude/settings.json missing"
fi

# Test 13: .github/labels.yml
echo "Test 13: Install creates .github/labels.yml"
if [[ -f "$INSTALL_REPO/.github/labels.yml" ]]; then
  pass ".github/labels.yml exists"
else
  fail ".github/labels.yml missing"
fi

# Test 14: .loom/roles with multiple role files
echo "Test 14: Install creates .loom/roles with role files"
ROLE_COUNT=$(find "$INSTALL_REPO/.loom/roles" -name "*.md" 2>/dev/null | wc -l | tr -d ' ')
if [[ $ROLE_COUNT -gt 10 ]]; then
  pass ".loom/roles has $ROLE_COUNT role definition files"
else
  fail ".loom/roles has only $ROLE_COUNT files (expected >10)"
fi

# Test 15: .loom/scripts with helper scripts
echo "Test 15: Install creates .loom/scripts with helper scripts"
SCRIPT_COUNT=$(find "$INSTALL_REPO/.loom/scripts" -name "*.sh" 2>/dev/null | wc -l | tr -d ' ')
if [[ $SCRIPT_COUNT -gt 5 ]]; then
  pass ".loom/scripts has $SCRIPT_COUNT shell scripts"
else
  fail ".loom/scripts has only $SCRIPT_COUNT scripts (expected >5)"
fi

# Test 16: .loom/hooks/guard-destructive.sh
echo "Test 16: Install creates .loom/hooks/guard-destructive.sh"
if [[ -f "$INSTALL_REPO/.loom/hooks/guard-destructive.sh" ]]; then
  if [[ -x "$INSTALL_REPO/.loom/hooks/guard-destructive.sh" ]]; then
    pass "guard-destructive.sh exists and is executable"
  else
    fail "guard-destructive.sh exists but is not executable"
  fi
else
  fail "guard-destructive.sh missing"
fi

# Test 17: .loom/config.json
echo "Test 17: Install creates .loom/config.json"
if [[ -f "$INSTALL_REPO/.loom/config.json" ]]; then
  pass ".loom/config.json exists"
else
  fail ".loom/config.json missing"
fi

# Test 18: .gitignore contains Loom patterns
echo "Test 18: .gitignore contains Loom runtime patterns"
if grep -q "Loom" "$INSTALL_REPO/.gitignore" 2>/dev/null; then
  pass ".gitignore contains Loom patterns"
else
  fail ".gitignore missing Loom patterns"
fi

# Test 19: Working tree is clean after simulated install
echo "Test 19: Working tree is clean after install"
if git -C "$INSTALL_REPO" diff --quiet 2>/dev/null && \
   git -C "$INSTALL_REPO" diff --staged --quiet 2>/dev/null; then
  pass "Working tree is clean"
else
  fail "Working tree has uncommitted changes"
fi
echo ""


# ==========================================================================
# Section 3: Uninstall Tests (--yes --local)
# ==========================================================================
echo "--- Section 3: Uninstall Tests ---"
echo ""

UNINSTALL_REPO="$TEST_DIR/uninstall-test"
create_temp_repo "$UNINSTALL_REPO"
simulate_loom_install "$UNINSTALL_REPO"

# Test 20: Uninstall completes successfully
echo "Test 20: Uninstall --yes --local completes"
if "$UNINSTALL_SCRIPT" --yes --local "$UNINSTALL_REPO" > /dev/null 2>&1; then
  pass "uninstall --yes --local completed successfully"
else
  fail "uninstall --yes --local failed"
fi

# Test 21: .loom/roles removed
echo "Test 21: After uninstall, .loom/roles removed"
REMAINING_ROLES=$(find "$UNINSTALL_REPO/.loom/roles" -type f 2>/dev/null | wc -l | tr -d ' ')
if [[ "$REMAINING_ROLES" -eq 0 ]] || [[ ! -d "$UNINSTALL_REPO/.loom/roles" ]]; then
  pass ".loom/roles cleaned up"
else
  fail ".loom/roles still has $REMAINING_ROLES files"
fi

# Test 22: .loom/scripts removed
echo "Test 22: After uninstall, .loom/scripts removed"
REMAINING_SCRIPTS=$(find "$UNINSTALL_REPO/.loom/scripts" -type f 2>/dev/null | wc -l | tr -d ' ')
if [[ "$REMAINING_SCRIPTS" -eq 0 ]] || [[ ! -d "$UNINSTALL_REPO/.loom/scripts" ]]; then
  pass ".loom/scripts cleaned up"
else
  fail ".loom/scripts still has $REMAINING_SCRIPTS files"
fi

# Test 23: .claude directory removed
echo "Test 23: After uninstall, .claude removed"
REMAINING_CLAUDE=$(find "$UNINSTALL_REPO/.claude" -type f 2>/dev/null | wc -l | tr -d ' ')
if [[ "$REMAINING_CLAUDE" -eq 0 ]] || [[ ! -d "$UNINSTALL_REPO/.claude" ]]; then
  pass ".claude directory cleaned up"
else
  fail ".claude still has $REMAINING_CLAUDE files"
fi

# Test 24: .github/labels.yml removed
echo "Test 24: After uninstall, .github/labels.yml removed"
if [[ ! -f "$UNINSTALL_REPO/.github/labels.yml" ]]; then
  pass ".github/labels.yml removed"
else
  fail ".github/labels.yml still exists"
fi

# Test 25: CLAUDE.md removed (Loom-generated)
echo "Test 25: After uninstall, CLAUDE.md removed"
if [[ ! -f "$UNINSTALL_REPO/CLAUDE.md" ]]; then
  pass "CLAUDE.md removed (Loom-generated)"
else
  # Check if Loom content was removed
  if grep -q "Loom Orchestration" "$UNINSTALL_REPO/CLAUDE.md" 2>/dev/null; then
    fail "CLAUDE.md still contains Loom content"
  else
    pass "CLAUDE.md Loom content removed"
  fi
fi

# Test 26: .loom/config.json removed (runtime artifact)
echo "Test 26: After uninstall, .loom/config.json removed"
if [[ ! -f "$UNINSTALL_REPO/.loom/config.json" ]]; then
  pass ".loom/config.json removed"
else
  fail ".loom/config.json still exists"
fi

# Test 26b: .loom/bin removed
echo "Test 26b: After uninstall, .loom/bin removed"
if [[ ! -f "$UNINSTALL_REPO/.loom/bin/loom" ]] || [[ ! -d "$UNINSTALL_REPO/.loom/bin" ]]; then
  pass ".loom/bin/loom cleaned up"
else
  fail ".loom/bin/loom still exists"
fi
echo ""


# ==========================================================================
# Section 4: Custom File Preservation
# ==========================================================================
echo "--- Section 4: Custom File Preservation ---"
echo ""

# Test 27: Non-clean uninstall preserves unknown files
echo "Test 27: Uninstall --yes preserves custom files (non-clean)"
CUSTOM_REPO="$TEST_DIR/custom-test"
create_temp_repo "$CUSTOM_REPO"
simulate_loom_install "$CUSTOM_REPO"

# Add custom files inside Loom-managed directories
mkdir -p "$CUSTOM_REPO/.loom/roles"
echo "custom role" > "$CUSTOM_REPO/.loom/roles/my-custom-role.md"
mkdir -p "$CUSTOM_REPO/.claude/commands"
echo "custom command" > "$CUSTOM_REPO/.claude/commands/my-custom-cmd.md"
git -C "$CUSTOM_REPO" add -A
git -C "$CUSTOM_REPO" commit -m "Add custom files" --quiet

"$UNINSTALL_SCRIPT" --yes --local "$CUSTOM_REPO" > /dev/null 2>&1 || true

if [[ -f "$CUSTOM_REPO/.loom/roles/my-custom-role.md" ]]; then
  pass "Custom role file preserved in non-clean mode"
else
  fail "Custom role file was removed in non-clean mode"
fi

if [[ -f "$CUSTOM_REPO/.claude/commands/my-custom-cmd.md" ]]; then
  pass "Custom command file preserved in non-clean mode"
else
  fail "Custom command file was removed in non-clean mode"
fi

# Test 28: Clean uninstall removes all files including custom
echo "Test 28: Uninstall --yes --clean removes custom files"
CLEAN_REPO="$TEST_DIR/clean-test"
create_temp_repo "$CLEAN_REPO"
simulate_loom_install "$CLEAN_REPO"

echo "custom config" > "$CLEAN_REPO/.loom/my-custom-config.txt"
mkdir -p "$CLEAN_REPO/.loom/roles"
echo "custom role" > "$CLEAN_REPO/.loom/roles/my-custom-role.md"
git -C "$CLEAN_REPO" add -A
git -C "$CLEAN_REPO" commit -m "Add custom file" --quiet

"$UNINSTALL_SCRIPT" --yes --local --clean "$CLEAN_REPO" > /dev/null 2>&1 || true

if [[ ! -f "$CLEAN_REPO/.loom/roles/my-custom-role.md" ]]; then
  pass "Custom role file removed in clean mode"
else
  fail "Custom role file preserved in clean mode (should be removed)"
fi
echo ""


# ==========================================================================
# Section 5: Reinstall Cycle
# ==========================================================================
echo "--- Section 5: Reinstall Cycle ---"
echo ""

# Test 29: Uninstall then reinstall cycle
echo "Test 29: Full uninstall-then-reinstall cycle"
REINSTALL_REPO="$TEST_DIR/reinstall-test"
create_temp_repo "$REINSTALL_REPO"
simulate_loom_install "$REINSTALL_REPO"

# Uninstall
"$UNINSTALL_SCRIPT" --yes --local --clean "$REINSTALL_REPO" > /dev/null 2>&1 || true
git -C "$REINSTALL_REPO" add -A
git -C "$REINSTALL_REPO" commit -m "Uninstall Loom" --quiet 2>/dev/null || true

# Verify key Loom files removed after uninstall
if [[ ! -d "$REINSTALL_REPO/.loom/roles" ]] && \
   [[ ! -d "$REINSTALL_REPO/.loom/scripts" ]] && \
   [[ ! -f "$REINSTALL_REPO/.loom/config.json" ]]; then
  pass "Key Loom directories removed after uninstall"
else
  fail "Uninstall left key Loom files behind"
fi

# Reinstall (simulated)
simulate_loom_install "$REINSTALL_REPO"

if [[ -d "$REINSTALL_REPO/.loom/roles" ]] && \
   [[ -f "$REINSTALL_REPO/CLAUDE.md" ]] && \
   [[ -d "$REINSTALL_REPO/.claude/commands" ]]; then
  pass "Reinstall cycle completed successfully"
else
  fail "Reinstall cycle left incomplete state"
fi

# Test 30: Reinstall preserves existing user content
echo "Test 30: Reinstall over existing preserves custom user files"
PRESERVE_REPO="$TEST_DIR/preserve-test"
create_temp_repo "$PRESERVE_REPO"
simulate_loom_install "$PRESERVE_REPO"

# Add user content outside Loom directories
echo "My project README" > "$PRESERVE_REPO/README.md"
echo "my_setting: true" > "$PRESERVE_REPO/.myconfig"
git -C "$PRESERVE_REPO" add -A
git -C "$PRESERVE_REPO" commit -m "Add user content" --quiet

# Uninstall and reinstall
"$UNINSTALL_SCRIPT" --yes --local --clean "$PRESERVE_REPO" > /dev/null 2>&1 || true
git -C "$PRESERVE_REPO" add -A
git -C "$PRESERVE_REPO" commit -m "Uninstall" --quiet 2>/dev/null || true
simulate_loom_install "$PRESERVE_REPO"

if [[ -f "$PRESERVE_REPO/README.md" ]] && grep -q "My project README" "$PRESERVE_REPO/README.md"; then
  pass "User README.md preserved through reinstall cycle"
else
  fail "User README.md was lost during reinstall"
fi

if [[ -f "$PRESERVE_REPO/.myconfig" ]]; then
  pass "User config file preserved through reinstall cycle"
else
  fail "User config file was lost during reinstall"
fi
echo ""


# ==========================================================================
# Section 6: CLAUDE.md Smart Removal
# ==========================================================================
echo "--- Section 6: CLAUDE.md Smart Removal ---"
echo ""

# Test 31: Loom-generated CLAUDE.md is fully removed
echo "Test 31: Loom-generated CLAUDE.md is fully removed"
CLAUDEMD_REPO="$TEST_DIR/claudemd-test"
create_temp_repo "$CLAUDEMD_REPO"
simulate_loom_install "$CLAUDEMD_REPO"

"$UNINSTALL_SCRIPT" --yes --local "$CLAUDEMD_REPO" > /dev/null 2>&1 || true

if [[ ! -f "$CLAUDEMD_REPO/CLAUDE.md" ]]; then
  pass "Loom-generated CLAUDE.md fully removed"
else
  fail "Loom-generated CLAUDE.md still exists"
fi

# Test 32: Mixed CLAUDE.md preserves user content (marker-based)
echo "Test 32: Mixed CLAUDE.md preserves user content"
MIXED_REPO="$TEST_DIR/mixed-claudemd-test"
create_temp_repo "$MIXED_REPO"
simulate_loom_install "$MIXED_REPO"

# Replace CLAUDE.md with mixed content using BEGIN/END markers
cat > "$MIXED_REPO/CLAUDE.md" << 'MIXED_EOF'
# My Project Instructions

These are my custom project instructions.

<!-- BEGIN LOOM ORCHESTRATION -->
# Loom Orchestration - Repository Guide

This is Loom content that should be removed.

Generated by Loom Installation Process
<!-- END LOOM ORCHESTRATION -->

## My Custom Section

Keep this content.
MIXED_EOF

git -C "$MIXED_REPO" add -A
git -C "$MIXED_REPO" commit -m "Mixed CLAUDE.md" --quiet

"$UNINSTALL_SCRIPT" --yes --local "$MIXED_REPO" > /dev/null 2>&1 || true

if [[ -f "$MIXED_REPO/CLAUDE.md" ]]; then
  if grep -q "My Project Instructions" "$MIXED_REPO/CLAUDE.md" && \
     grep -q "My Custom Section" "$MIXED_REPO/CLAUDE.md"; then
    if ! grep -q "Loom Orchestration" "$MIXED_REPO/CLAUDE.md"; then
      pass "Mixed CLAUDE.md: user content preserved, Loom section removed"
    else
      fail "Mixed CLAUDE.md: Loom section not fully removed"
    fi
  else
    fail "Mixed CLAUDE.md: user content was lost"
  fi
else
  fail "Mixed CLAUDE.md: entire file was removed (should preserve user content)"
fi
echo ""


# ==========================================================================
# Summary
# ==========================================================================
echo "======================================"
echo "Test Summary"
echo "======================================"
echo -e "${GREEN}Passed: $passed${NC}"
echo -e "${RED}Failed: $failed${NC}"
echo ""

if [ $failed -eq 0 ]; then
  echo -e "${GREEN}All tests passed!${NC}"
  exit 0
else
  echo -e "${RED}Some tests failed.${NC}"
  exit 1
fi
