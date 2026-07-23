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
WRAPPER_SCRIPT="$LOOM_ROOT/install.sh"
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
  # Honor defaults/.loom-internal.list (#3464) so this simulator matches the
  # real installer's ownership boundary — files listed in .loom-internal.list
  # are Loom-internal and not shipped to consumer repos.
  if [[ -d "$DEFAULTS_DIR/.claude" ]]; then
    cp -r "$DEFAULTS_DIR/.claude" "$target/.claude"
    if [[ -r "$DEFAULTS_DIR/.loom-internal.list" ]]; then
      while IFS= read -r skip_rel; do
        skip_rel="${skip_rel%%#*}"
        # shellcheck disable=SC2295
        skip_rel="${skip_rel#"${skip_rel%%[![:space:]]*}"}"
        skip_rel="${skip_rel%"${skip_rel##*[![:space:]]}"}"
        [[ -z "$skip_rel" ]] && continue
        if [[ -e "$target/$skip_rel" ]]; then
          rm -f "$target/$skip_rel"
        fi
      done < "$DEFAULTS_DIR/.loom-internal.list"
    fi
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

  # Build installed_files manifest by collecting all files we just installed
  local installed_files_json="["
  local first_file=true
  while IFS= read -r -d '' file; do
    local rel_path="${file#$target/}"
    # Skip runtime artifacts and metadata
    case "$rel_path" in
      .loom/install-metadata.json|.loom/state.json|.loom/daemon-state.json|.loom/loom-source-path|.loom/manifest.json)
        continue
        ;;
    esac
    if [[ "$first_file" == "true" ]]; then
      first_file=false
    else
      installed_files_json="${installed_files_json},"
    fi
    installed_files_json="${installed_files_json}\"${rel_path}\""
  done < <(find \
    "$target/.loom" "$target/.claude" "$target/.codex" "$target/.github" \
    "$target/.githooks" "$target/CLAUDE.md" "$target/.gitignore" \
    -maxdepth 20 -type f \
    -not -path "$target/.loom/worktrees/*" \
    -not -name '.DS_Store' \
    -not -name '*.log' \
    -not -name '*.sock' \
    2>/dev/null \
    -print0 | sort -z)
  installed_files_json="${installed_files_json}]"

  # Create install metadata with installed_files manifest
  cat > "$target/.loom/install-metadata.json" <<META_EOF
{
  "loom_version": "0.0.0-test",
  "loom_commit": "test",
  "install_date": "2026-01-01",
  "loom_source": "/tmp/test-loom",
  "installed_files": ${installed_files_json}
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

# Test 11: .claude/commands/loom
echo "Test 11: Install creates .claude/commands/loom"
if [[ -d "$INSTALL_REPO/.claude/commands/loom" ]]; then
  pass ".claude/commands/loom directory exists"
else
  fail ".claude/commands/loom directory missing"
fi

# Test 12: .claude/settings.json
echo "Test 12: Install creates .claude/settings.json"
if [[ -f "$INSTALL_REPO/.claude/settings.json" ]]; then
  pass ".claude/settings.json exists"
else
  fail ".claude/settings.json missing"
fi

# Test 12b: Hook commands use ${CLAUDE_PROJECT_DIR} prefix (issue #3265)
# Hooks must use ${CLAUDE_PROJECT_DIR}/.loom/hooks/... so they resolve regardless
# of the agent's current working directory. Bare-relative paths fail when the
# session cwd has moved into a subdirectory.
echo "Test 12b: Hook commands use \${CLAUDE_PROJECT_DIR} prefix"
SETTINGS_FILE="$INSTALL_REPO/.claude/settings.json"
if [[ -f "$SETTINGS_FILE" ]] && command -v jq &> /dev/null; then
  HOOK_PREFIX_FAIL=0
  for hook_name in guard-destructive.sh guard-loom-workflow.sh skill-router.sh methodology-inject.sh; do
    # Collect every command in the settings.json that ends with this hook script.
    matches=$(jq -r --arg name "$hook_name" \
      '[.. | objects | select(.command? != null) | .command | select(endswith($name))][]' \
      "$SETTINGS_FILE" 2>/dev/null)
    if [[ -z "$matches" ]]; then
      fail "Hook command for $hook_name not found in settings.json"
      HOOK_PREFIX_FAIL=1
      continue
    fi
    while IFS= read -r cmd; do
      [[ -z "$cmd" ]] && continue
      # The literal string '${CLAUDE_PROJECT_DIR}' must appear at the start
      # (not the expanded value -- the JSON stores the placeholder verbatim
      # and Claude Code expands it at hook-invocation time).
      if [[ "$cmd" != '${CLAUDE_PROJECT_DIR}/.loom/hooks/'* ]]; then
        fail "Hook command does not use \${CLAUDE_PROJECT_DIR} prefix: $cmd"
        HOOK_PREFIX_FAIL=1
      fi
    done <<< "$matches"
  done
  if [[ $HOOK_PREFIX_FAIL -eq 0 ]]; then
    pass "All Loom hook commands use \${CLAUDE_PROJECT_DIR} prefix"
  fi
else
  fail "Cannot verify hook command prefixes (settings.json or jq missing)"
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

# Test 15b: .loom/scripts/lib/ subdirectory and its helpers (regression test for #3220)
# These files are sourced by ~17 other scripts (merge-pr.sh, agent-spawn.sh, etc.)
# and must always be present after a successful install.
echo "Test 15b: Install creates .loom/scripts/lib/ with all required helpers"
LIB_DIR="$INSTALL_REPO/.loom/scripts/lib"
LIB_MISSING=0
if [[ ! -d "$LIB_DIR" ]]; then
  fail ".loom/scripts/lib/ directory missing (regression of #3220)"
  LIB_MISSING=1
else
  for lib_file in loom-tools.sh forge-helpers.sh pipe-pane-cmd.sh; do
    if [[ ! -f "$LIB_DIR/$lib_file" ]]; then
      fail ".loom/scripts/lib/$lib_file missing (regression of #3220)"
      LIB_MISSING=1
    fi
  done
  if [[ $LIB_MISSING -eq 0 ]]; then
    pass ".loom/scripts/lib/ contains all required helpers (loom-tools.sh, forge-helpers.sh, pipe-pane-cmd.sh)"
  fi
fi

# Test 15c: every file in defaults/scripts/ exists in installed .loom/scripts/
# This is a structural check that catches future regressions like #3220 where
# new files added to defaults/scripts/ might not reach the install target.
echo "Test 15c: All files in defaults/scripts/ are installed under .loom/scripts/"
SCRIPTS_MISSING_COUNT=0
SCRIPTS_MISSING_LIST=""
if [[ -d "$DEFAULTS_DIR/scripts" ]]; then
  while IFS= read -r -d '' src_file; do
    rel_path="${src_file#$DEFAULTS_DIR/scripts/}"
    dst_file="$INSTALL_REPO/.loom/scripts/$rel_path"
    if [[ ! -f "$dst_file" ]]; then
      SCRIPTS_MISSING_COUNT=$((SCRIPTS_MISSING_COUNT + 1))
      SCRIPTS_MISSING_LIST="${SCRIPTS_MISSING_LIST}\n  - $rel_path"
    fi
  done < <(find "$DEFAULTS_DIR/scripts" -type f -print0)
fi
if [[ $SCRIPTS_MISSING_COUNT -eq 0 ]]; then
  pass "All defaults/scripts/ files installed (recursive parity check)"
else
  fail "$SCRIPTS_MISSING_COUNT script(s) from defaults/scripts/ missing in install:$(printf '%b' "$SCRIPTS_MISSING_LIST")"
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

# Test 16b: .loom/hooks/guard-loom-workflow.sh (issue #3604)
echo "Test 16b: Install creates .loom/hooks/guard-loom-workflow.sh"
if [[ -f "$INSTALL_REPO/.loom/hooks/guard-loom-workflow.sh" ]]; then
  if [[ -x "$INSTALL_REPO/.loom/hooks/guard-loom-workflow.sh" ]]; then
    pass "guard-loom-workflow.sh exists and is executable"
  else
    fail "guard-loom-workflow.sh exists but is not executable"
  fi
else
  fail "guard-loom-workflow.sh missing"
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

# Test 28: Clean uninstall removes Loom-owned custom files but preserves shared dir custom files
echo "Test 28: Uninstall --yes --clean removes Loom-owned custom files"
CLEAN_REPO="$TEST_DIR/clean-test"
create_temp_repo "$CLEAN_REPO"
simulate_loom_install "$CLEAN_REPO"

echo "custom config" > "$CLEAN_REPO/.loom/my-custom-config.txt"
mkdir -p "$CLEAN_REPO/.loom/roles"
echo "custom role" > "$CLEAN_REPO/.loom/roles/my-custom-role.md"
mkdir -p "$CLEAN_REPO/.claude/commands"
echo "custom command" > "$CLEAN_REPO/.claude/commands/my-custom-cmd.md"
mkdir -p "$CLEAN_REPO/.claude/agents"
echo "custom agent" > "$CLEAN_REPO/.claude/agents/my-custom-agent.md"
git -C "$CLEAN_REPO" add -A
git -C "$CLEAN_REPO" commit -m "Add custom file" --quiet

"$UNINSTALL_SCRIPT" --yes --local --clean "$CLEAN_REPO" > /dev/null 2>&1 || true

if [[ ! -f "$CLEAN_REPO/.loom/roles/my-custom-role.md" ]]; then
  pass "Custom role in Loom-owned dir removed in clean mode"
else
  fail "Custom role in Loom-owned dir preserved in clean mode (should be removed)"
fi

# Test 28b: Custom commands in shared directories (.claude/) preserved even in clean mode
echo "Test 28b: Uninstall --clean preserves custom commands in shared directories"
if [[ -f "$CLEAN_REPO/.claude/commands/my-custom-cmd.md" ]]; then
  pass "Custom command in .claude/commands/ preserved in clean mode"
else
  fail "Custom command in .claude/commands/ removed in clean mode (should be preserved)"
fi

if [[ -f "$CLEAN_REPO/.claude/agents/my-custom-agent.md" ]]; then
  pass "Custom agent in .claude/agents/ preserved in clean mode"
else
  fail "Custom agent in .claude/agents/ removed in clean mode (should be preserved)"
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
   [[ -d "$REINSTALL_REPO/.claude/commands/loom" ]]; then
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
# Section 7: Project-Specific Files in .loom/ Subdirectories
# ==========================================================================
echo "--- Section 7: Project-Specific Files in .loom/ ---"
echo ""

# Test 33: Project-specific directories in .loom/ survive uninstall
echo "Test 33: Project dirs in .loom/ survive uninstall (manifest-based)"
PROJECT_REPO="$TEST_DIR/project-dirs-test"
create_temp_repo "$PROJECT_REPO"
simulate_loom_install "$PROJECT_REPO"

# Create project-specific directories and files inside .loom/
# These simulate real-world usage (e.g., sphere's claims/, diagnostics/)
mkdir -p "$PROJECT_REPO/.loom/claims"
echo '{"claim": "test"}' > "$PROJECT_REPO/.loom/claims/claim-1.json"
mkdir -p "$PROJECT_REPO/.loom/diagnostics"
echo "diagnostic data" > "$PROJECT_REPO/.loom/diagnostics/report.txt"
mkdir -p "$PROJECT_REPO/.loom/methodology-cache"
echo "cached data" > "$PROJECT_REPO/.loom/methodology-cache/cache.json"
mkdir -p "$PROJECT_REPO/.loom/tests"
echo "test config" > "$PROJECT_REPO/.loom/tests/test-config.json"

# Also create project-specific hooks in .claude/hooks/
mkdir -p "$PROJECT_REPO/.claude/hooks"
echo '#!/bin/bash' > "$PROJECT_REPO/.claude/hooks/guard-pdk-files.sh"
echo '#!/bin/bash' > "$PROJECT_REPO/.claude/hooks/skill-router.sh"

# And project-specific agents
mkdir -p "$PROJECT_REPO/.claude/agents"
echo "# AMS Architect" > "$PROJECT_REPO/.claude/agents/ams-architect.md"
echo "# Layout Place" > "$PROJECT_REPO/.claude/agents/layout-place.md"

git -C "$PROJECT_REPO" add -A
git -C "$PROJECT_REPO" commit -m "Add project-specific files" --quiet

# Run uninstall (non-interactive, non-clean)
"$UNINSTALL_SCRIPT" --yes --local "$PROJECT_REPO" > /dev/null 2>&1 || true

# Verify project-specific directories and files survived
if [[ -f "$PROJECT_REPO/.loom/claims/claim-1.json" ]]; then
  pass "Project dir .loom/claims/ preserved after uninstall"
else
  fail "Project dir .loom/claims/ was removed by uninstall"
fi

if [[ -f "$PROJECT_REPO/.loom/diagnostics/report.txt" ]]; then
  pass "Project dir .loom/diagnostics/ preserved after uninstall"
else
  fail "Project dir .loom/diagnostics/ was removed by uninstall"
fi

if [[ -f "$PROJECT_REPO/.loom/methodology-cache/cache.json" ]]; then
  pass "Project dir .loom/methodology-cache/ preserved after uninstall"
else
  fail "Project dir .loom/methodology-cache/ was removed by uninstall"
fi

if [[ -f "$PROJECT_REPO/.loom/tests/test-config.json" ]]; then
  pass "Project dir .loom/tests/ preserved after uninstall"
else
  fail "Project dir .loom/tests/ was removed by uninstall"
fi

# Verify project-specific hooks survived
if [[ -f "$PROJECT_REPO/.claude/hooks/guard-pdk-files.sh" ]]; then
  pass "Project hook .claude/hooks/guard-pdk-files.sh preserved"
else
  fail "Project hook .claude/hooks/guard-pdk-files.sh was removed"
fi

if [[ -f "$PROJECT_REPO/.claude/hooks/skill-router.sh" ]]; then
  pass "Project hook .claude/hooks/skill-router.sh preserved"
else
  fail "Project hook .claude/hooks/skill-router.sh was removed"
fi

# Verify project-specific agents survived
if [[ -f "$PROJECT_REPO/.claude/agents/ams-architect.md" ]]; then
  pass "Project agent .claude/agents/ams-architect.md preserved"
else
  fail "Project agent .claude/agents/ams-architect.md was removed"
fi

# Verify Loom files WERE removed
if [[ ! -d "$PROJECT_REPO/.loom/roles" ]] || [[ $(find "$PROJECT_REPO/.loom/roles" -type f 2>/dev/null | wc -l | tr -d ' ') -eq 0 ]]; then
  pass "Loom roles were correctly removed"
else
  fail "Loom roles were not removed"
fi

if [[ ! -d "$PROJECT_REPO/.loom/scripts" ]] || [[ $(find "$PROJECT_REPO/.loom/scripts" -type f 2>/dev/null | wc -l | tr -d ' ') -eq 0 ]]; then
  pass "Loom scripts were correctly removed"
else
  fail "Loom scripts were not removed"
fi

# Test 34: No "Preserved directory" noise (uninstall output check)
echo "Test 34: No 'Preserved directory' noise in uninstall output"
NOISE_REPO="$TEST_DIR/noise-test"
create_temp_repo "$NOISE_REPO"
simulate_loom_install "$NOISE_REPO"
mkdir -p "$NOISE_REPO/.loom/project-data"
echo "data" > "$NOISE_REPO/.loom/project-data/info.txt"
git -C "$NOISE_REPO" add -A
git -C "$NOISE_REPO" commit -m "Add project data" --quiet

UNINSTALL_OUTPUT=$("$UNINSTALL_SCRIPT" --yes --local "$NOISE_REPO" 2>&1 || true)
if echo "$UNINSTALL_OUTPUT" | grep -q "Preserved directory"; then
  fail "Uninstall output contains 'Preserved directory' noise"
else
  pass "No 'Preserved directory' noise in uninstall output"
fi
echo ""


# ==========================================================================
# Section 35-38: Post-install verification snapshot diff (issue #3219)
# ==========================================================================
# These tests exercise the snapshot-comparison math used by install-loom.sh
# to distinguish installer-introduced residue from the user's pre-existing
# dirty working tree. The math is short enough to mirror inline; if it
# drifts from install-loom.sh, both must be updated.

# Helper: replicate the symmetric-difference logic from install-loom.sh
diff_snapshot() {
  local pre="$1"
  local post="$2"
  if [[ -z "$post" ]]; then
    return 0
  fi
  if [[ -z "$pre" ]]; then
    printf '%s' "$post"
    return 0
  fi
  printf '%s\n' "$post" | grep -F -x -v -f <(printf '%s\n' "$pre") || true
}

echo "--- Section: Post-install verification snapshot diff (#3219) ---"
echo ""

# Test 35: empty pre, empty post -> empty diff (clean repo, clean after)
echo "Test 35: Empty snapshot, empty post-state yields empty diff"
RESULT=$(diff_snapshot "" "")
if [[ -z "$RESULT" ]]; then
  pass "Clean -> clean produces no residue"
else
  fail "Clean -> clean unexpectedly produced: $RESULT"
fi
echo ""

# Test 36: pre-existing dirty state with no install changes -> empty diff
echo "Test 36: Pre-existing dirty entries are filtered out of residue"
PRE=' M README.md
?? local-notes.txt'
POST=' M README.md
?? local-notes.txt'
RESULT=$(diff_snapshot "$PRE" "$POST")
if [[ -z "$RESULT" ]]; then
  pass "User's pre-existing dirty state does not register as residue"
else
  fail "Pre-existing entries leaked into residue: $RESULT"
fi
echo ""

# Test 37: genuine new install residue is detected
echo "Test 37: New install-introduced entries surface in residue"
PRE=' M README.md'
POST=' M README.md
 M .loom/config.json
?? .loom/loom-source-path'
RESULT=$(diff_snapshot "$PRE" "$POST")
EXPECTED=' M .loom/config.json
?? .loom/loom-source-path'
if [[ "$RESULT" == "$EXPECTED" ]]; then
  pass "Install-introduced entries appear in residue"
else
  fail "Residue mismatch. Got: [$RESULT] Expected: [$EXPECTED]"
fi
echo ""

# Test 38: empty pre with non-empty post returns full post (initial-clean repo
# that is dirty after install)
echo "Test 38: Empty pre-snapshot returns full post-state as residue"
POST=' M .loom/config.json
?? .loom/loom-source-path'
RESULT=$(diff_snapshot "" "$POST")
if [[ "$RESULT" == "$POST" ]]; then
  pass "Empty snapshot treats all post-entries as new"
else
  fail "Empty snapshot mishandled. Got: [$RESULT]"
fi
echo ""

# Test 39: gh pr create passes --head explicitly (regression for #3244)
# Without --head, gh tries to auto-detect from origin and can fail in shells
# where its host detection is degraded, even with -R already set.
echo "Test 39: create-pr.sh passes --head to gh pr create"
if grep -A8 'gh pr create \\' "$LOOM_ROOT/scripts/install/create-pr.sh" | \
     grep -q -- '--head "\$BRANCH_NAME"'; then
  pass "create-pr.sh's gh pr create includes --head \$BRANCH_NAME"
else
  fail "create-pr.sh's gh pr create is missing --head — would orphan remote branches when origin auto-detect fails (#3244)"
fi
echo ""

# Test 40: install-loom.sh cleanup_on_error deletes the orphan remote branch
# when the install fails after push but before PR creation completes (#3244).
echo "Test 40: cleanup_on_error deletes orphan remote install branches"
if grep -q 'git push origin --delete "\${BRANCH_NAME}"' "$INSTALL_SCRIPT"; then
  pass "cleanup_on_error deletes orphan remote branches"
else
  fail "cleanup_on_error is missing remote-branch cleanup for orphaned install branches (#3244)"
fi
echo ""

# Test 41: Remote-branch cleanup is prefix-restricted to feature/loom-install-v*
# so a branch like 'topic/feature/loom-install-v0.7.0' wouldn't match.
echo "Test 41: remote-branch cleanup is restricted to feature/loom-install-v* prefix"
if grep -q '"\${BRANCH_NAME}" =~ \^feature/loom-install-v' "$INSTALL_SCRIPT"; then
  pass "Cleanup regex is anchored at start of branch name (^feature/loom-install-v)"
else
  fail "Cleanup regex is not anchored — could delete unrelated branches"
fi
echo ""


# ==========================================================================
# Section 5: Stale-File Sweep (Upgrade Path)
# ==========================================================================
# These tests exercise the stale-file sweep logic from install-loom.sh
# (the "Stale-file sweep (upgrade path)" loop). The sweep reads the previous
# install's installed_files list from .loom/install-metadata.json, compares it
# against the new set, and git-rm's any files present in the old list but
# absent from the new list.
# The logic is mirrored inline here (like the diff_snapshot tests above) to
# allow isolated verification without invoking the full install workflow.
#
# Helper: replicate the stale-file identification logic from install-loom.sh,
# INCLUDING the consumer-owned carve-out (#3450, #3480). Keep the case
# statement in sync with the one in install-loom.sh.
# Arguments:
#   $1  - path to install-metadata.json (may not exist)
#   $2  - INSTALLED_FILES_JSON string (the new install's file list as JSON)
# Prints one stale file path per line (empty output = no stale files).
find_stale_files() {
  local metadata_file="$1"
  local new_files_json="$2"
  if [[ ! -f "$metadata_file" ]]; then
    return 0
  fi
  if ! command -v jq >/dev/null 2>&1; then
    return 0
  fi
  while IFS= read -r prev_file; do
    [[ -n "$prev_file" ]] || continue
    # Mirror of the consumer-owned carve-out in install-loom.sh: .github/
    # is an allowlist of Loom-shipped files; anything else under .github/
    # is consumer-owned by default and never swept.
    case "$prev_file" in
      CLAUDE.md|.gitignore|.claude/settings.json)
        continue
        ;;
      .github/labels.yml|.github/CONFIGURATION.md|.github/ISSUE_TEMPLATE/config.yml|.github/ISSUE_TEMPLATE/task.yml)
        # Loom-shipped — fall through to the sweep.
        ;;
      .github/*)
        # Consumer-owned by default.
        continue
        ;;
    esac
    if ! echo "$new_files_json" | grep -qF "\"${prev_file}\""; then
      echo "$prev_file"
    fi
  done < <(jq -r '.installed_files[]' "$metadata_file")
}

# Guard: the carve-out case statement above is a hand-maintained mirror of the
# one in install-loom.sh. Fail loudly if the allowlist drifts.
assert_carveout_in_sync() {
  local expected=".github/labels.yml|.github/CONFIGURATION.md|.github/ISSUE_TEMPLATE/config.yml|.github/ISSUE_TEMPLATE/task.yml"
  if grep -qF "$expected" "$SCRIPT_DIR/install-loom.sh" \
    && grep -qF "$expected" "$SCRIPT_DIR/uninstall-loom.sh"; then
    pass "Carve-out allowlist present in install-loom.sh and uninstall-loom.sh"
  else
    fail "Carve-out allowlist drifted between test mirror, install-loom.sh, and uninstall-loom.sh"
  fi
}

echo "--- Section 5: Stale-File Sweep (Upgrade Path) ---"
echo ""

# Test 42: Fresh install — no previous install-metadata.json → sweep is skipped
echo "Test 42: Fresh install (no previous metadata) skips stale-file sweep"
FRESH_SWEEP_REPO="$TEST_DIR/fresh-sweep-test"
create_temp_repo "$FRESH_SWEEP_REPO"
simulate_loom_install "$FRESH_SWEEP_REPO"
# Remove the metadata that simulate_loom_install wrote so we simulate
# a repo that has never been installed before (no metadata.json present).
rm -f "$FRESH_SWEEP_REPO/.loom/install-metadata.json"
NEW_FILES_JSON='[".loom/scripts/worktree.sh",".loom/roles/builder.md"]'
STALE=$(find_stale_files "$FRESH_SWEEP_REPO/.loom/install-metadata.json" "$NEW_FILES_JSON")
if [[ -z "$STALE" ]]; then
  pass "No stale files detected when metadata is absent (sweep skipped)"
else
  fail "Sweep returned stale files when metadata is absent: $STALE"
fi
echo ""

# Test 43: Upgrade with removals — file in old metadata absent from new set →
# that file is identified as stale and git-rm'd.
echo "Test 43: Upgrade removes file absent from new defaults"
UPGRADE_SWEEP_REPO="$TEST_DIR/upgrade-sweep-test"
create_temp_repo "$UPGRADE_SWEEP_REPO"
simulate_loom_install "$UPGRADE_SWEEP_REPO"

# Create a fake stale file that was in the previous install but is no longer
# shipped in the new defaults.  Commit it so git rm can remove it.
STALE_FILE=".loom/scripts/some-deleted-file.sh"
mkdir -p "$UPGRADE_SWEEP_REPO/.loom/scripts"
echo "#!/bin/bash" > "$UPGRADE_SWEEP_REPO/$STALE_FILE"
git -C "$UPGRADE_SWEEP_REPO" add "$STALE_FILE"
git -C "$UPGRADE_SWEEP_REPO" commit -m "Add stale script" --quiet

# Overwrite install-metadata.json to list the stale file as previously installed.
# Note: install-metadata.json is gitignored (runtime artifact); write directly to
# disk without committing, just as the real installer does.
cat > "$UPGRADE_SWEEP_REPO/.loom/install-metadata.json" <<EOF
{
  "loom_version": "0.0.0-old",
  "loom_commit": "old",
  "install_date": "2026-01-01",
  "loom_source": "$LOOM_ROOT",
  "installed_files": ["$STALE_FILE"]
}
EOF

# New install's file list does NOT include the stale file.
NEW_FILES_JSON='[".loom/scripts/worktree.sh",".loom/roles/builder.md"]'

# Identify stale files (mirrors the install-loom.sh identification step).
STALE=$(find_stale_files "$UPGRADE_SWEEP_REPO/.loom/install-metadata.json" "$NEW_FILES_JSON")
if [[ "$STALE" == "$STALE_FILE" ]]; then
  pass "Stale file correctly identified: $STALE_FILE"
else
  fail "Expected stale file '$STALE_FILE', got: '$STALE'"
fi

# Apply the sweep (mirrors install-loom.sh's git-rm step) and verify removal.
if [[ -n "$STALE" ]]; then
  while IFS= read -r f; do
    git -C "$UPGRADE_SWEEP_REPO" rm --quiet --force "$f" 2>/dev/null || true
  done <<< "$STALE"
fi
if [[ ! -f "$UPGRADE_SWEEP_REPO/$STALE_FILE" ]]; then
  pass "Stale file removed from working tree after sweep"
else
  fail "Stale file still present after sweep: $STALE_FILE"
fi
echo ""

# Test 44: Operator-added file — a file present on disk but NOT listed in the
# previous installed_files is NOT touched by the sweep.
echo "Test 44: Operator-added file not in previous metadata is preserved"
OPERATOR_SWEEP_REPO="$TEST_DIR/operator-sweep-test"
create_temp_repo "$OPERATOR_SWEEP_REPO"
simulate_loom_install "$OPERATOR_SWEEP_REPO"

# Operator adds a custom script after installation; it is never in installed_files.
OPERATOR_FILE=".loom/scripts/my-custom-helper.sh"
mkdir -p "$OPERATOR_SWEEP_REPO/.loom/scripts"
echo "#!/bin/bash" > "$OPERATOR_SWEEP_REPO/$OPERATOR_FILE"
git -C "$OPERATOR_SWEEP_REPO" add "$OPERATOR_FILE"
git -C "$OPERATOR_SWEEP_REPO" commit -m "Add operator custom helper" --quiet

# Previous metadata lists only a different (genuinely stale) file, not the
# operator file — exactly as would happen in a real upgrade scenario.
# Note: install-metadata.json is gitignored (runtime artifact); write directly to
# disk without committing, just as the real installer does.
PREV_STALE_FILE=".loom/scripts/old-removed-helper.sh"
cat > "$OPERATOR_SWEEP_REPO/.loom/install-metadata.json" <<EOF
{
  "loom_version": "0.0.0-old",
  "loom_commit": "old",
  "install_date": "2026-01-01",
  "loom_source": "$LOOM_ROOT",
  "installed_files": ["$PREV_STALE_FILE"]
}
EOF

# New install's file list also does not include $PREV_STALE_FILE (it was removed),
# and never mentioned $OPERATOR_FILE (it was operator-added).
NEW_FILES_JSON='[".loom/scripts/worktree.sh",".loom/roles/builder.md"]'

# Run the sweep: only $PREV_STALE_FILE should surface as stale.
STALE=$(find_stale_files "$OPERATOR_SWEEP_REPO/.loom/install-metadata.json" "$NEW_FILES_JSON")
if [[ "$STALE" == "$PREV_STALE_FILE" ]]; then
  pass "Only previously-installed stale file identified (not the operator file)"
else
  fail "Unexpected stale files: '$STALE' (expected only '$PREV_STALE_FILE')"
fi

# Verify operator file is not in the stale list.
if echo "$STALE" | grep -qF "$OPERATOR_FILE"; then
  fail "Operator-added file incorrectly flagged as stale: $OPERATOR_FILE"
else
  pass "Operator-added file not in stale list (safe by construction)"
fi

# After applying the sweep the operator file must still be on disk.
if [[ -n "$STALE" ]]; then
  while IFS= read -r f; do
    git -C "$OPERATOR_SWEEP_REPO" rm --quiet --force "$f" 2>/dev/null || true
  done <<< "$STALE"
fi
if [[ -f "$OPERATOR_SWEEP_REPO/$OPERATOR_FILE" ]]; then
  pass "Operator-added file preserved on disk after stale-file sweep"
else
  fail "Operator-added file was removed by stale-file sweep (should be preserved)"
fi
echo ""

# Test 44b: Consumer-owned .github/ files captured by an over-broad legacy
# manifest (v0.7.x, #3450) survive the sweep, while genuinely stale
# Loom-shipped .github/ files are still swept (#3480 — rjwalters/vibesql#5168).
echo "Test 44b: Consumer .github/ files in over-broad manifest survive sweep"
GITHUB_SWEEP_REPO="$TEST_DIR/github-sweep-test"
create_temp_repo "$GITHUB_SWEEP_REPO"
simulate_loom_install "$GITHUB_SWEEP_REPO"

# Consumer-owned .github files (the exact shapes deleted in vibesql#5168).
CONSUMER_ACTION=".github/actions/foo/action.yml"
CONSUMER_DEPENDABOT=".github/dependabot.yml"
CONSUMER_TOPLEVEL=".github/consumer.json"
mkdir -p "$GITHUB_SWEEP_REPO/.github/actions/foo"
echo "name: foo" > "$GITHUB_SWEEP_REPO/$CONSUMER_ACTION"
echo "version: 2" > "$GITHUB_SWEEP_REPO/$CONSUMER_DEPENDABOT"
echo "{}" > "$GITHUB_SWEEP_REPO/$CONSUMER_TOPLEVEL"

# A Loom-shipped .github file that the new version no longer ships — this
# one MUST still be swept (the allowlist lets it fall through).
STALE_LOOM_GH_FILE=".github/CONFIGURATION.md"
mkdir -p "$GITHUB_SWEEP_REPO/.github"
echo "# Loom configuration" > "$GITHUB_SWEEP_REPO/$STALE_LOOM_GH_FILE"

git -C "$GITHUB_SWEEP_REPO" add .github
git -C "$GITHUB_SWEEP_REPO" commit -m "Consumer .github files + legacy Loom file" --quiet

# Over-broad previous manifest: lists consumer files (the v0.7.x bug), the
# stale Loom-shipped file, and a still-shipped allowlisted file.
cat > "$GITHUB_SWEEP_REPO/.loom/install-metadata.json" <<EOF
{
  "loom_version": "0.7.1",
  "loom_commit": "old",
  "install_date": "2026-01-01",
  "loom_source": "$LOOM_ROOT",
  "installed_files": ["$CONSUMER_ACTION","$CONSUMER_DEPENDABOT","$CONSUMER_TOPLEVEL","$STALE_LOOM_GH_FILE",".github/labels.yml"]
}
EOF

# New install ships labels.yml but no longer ships CONFIGURATION.md, and of
# course never shipped the consumer files.
NEW_FILES_JSON='[".github/labels.yml",".loom/roles/builder.md"]'

STALE=$(find_stale_files "$GITHUB_SWEEP_REPO/.loom/install-metadata.json" "$NEW_FILES_JSON")

# Consumer files must NOT surface as stale.
GITHUB_SWEEP_OK=true
for consumer_file in "$CONSUMER_ACTION" "$CONSUMER_DEPENDABOT" "$CONSUMER_TOPLEVEL"; do
  if echo "$STALE" | grep -qF "$consumer_file"; then
    fail "Consumer-owned file incorrectly flagged as stale: $consumer_file"
    GITHUB_SWEEP_OK=false
  fi
done
if [[ "$GITHUB_SWEEP_OK" == "true" ]]; then
  pass "Consumer-owned .github/ files not flagged as stale (allowlist default-skip)"
fi

# The stale Loom-shipped .github file MUST surface as stale.
if echo "$STALE" | grep -qF "$STALE_LOOM_GH_FILE"; then
  pass "Stale Loom-shipped .github file still identified: $STALE_LOOM_GH_FILE"
else
  fail "Stale Loom-shipped .github file not identified (expected '$STALE_LOOM_GH_FILE' in: '$STALE')"
fi

# Allowlisted file present in both sets must NOT be flagged (regression).
if echo "$STALE" | grep -qF ".github/labels.yml"; then
  fail "Still-shipped .github/labels.yml incorrectly flagged as stale"
else
  pass "Still-shipped allowlisted file (.github/labels.yml) not flagged as stale"
fi

# Apply the sweep; consumer files survive on disk, stale Loom file is gone.
if [[ -n "$STALE" ]]; then
  while IFS= read -r f; do
    git -C "$GITHUB_SWEEP_REPO" rm --quiet --force "$f" 2>/dev/null || true
  done <<< "$STALE"
fi
if [[ -f "$GITHUB_SWEEP_REPO/$CONSUMER_ACTION" ]] \
  && [[ -f "$GITHUB_SWEEP_REPO/$CONSUMER_DEPENDABOT" ]] \
  && [[ -f "$GITHUB_SWEEP_REPO/$CONSUMER_TOPLEVEL" ]]; then
  pass "Consumer-owned .github/ files preserved on disk after sweep"
else
  fail "Consumer-owned .github/ file(s) deleted by sweep (vibesql#5168 regression)"
fi
if [[ ! -f "$GITHUB_SWEEP_REPO/$STALE_LOOM_GH_FILE" ]]; then
  pass "Stale Loom-shipped .github file removed by sweep"
else
  fail "Stale Loom-shipped .github file still present after sweep: $STALE_LOOM_GH_FILE"
fi

# Drift guard: the test mirror's allowlist must match both real scripts.
assert_carveout_in_sync
echo ""


# ==========================================================================
# Section 5b: Retired-File Cleanup (#3572)
# ==========================================================================
# Exercises the content-gated retired-file cleanup block in install-loom.sh
# (the "Retired-file cleanup (content-gated)" block after the stale-file
# sweep). A file on the frozen retired-file allowlist is git-rm'd ONLY when its
# on-disk content hashes to a shipped digest (unmodified); a consumer-modified
# copy is preserved; an absent file is a no-op. The gate logic is mirrored
# inline here (like find_stale_files above) so it can be verified without
# invoking the full installer, plus a drift guard that asserts the real
# allowlist in install-loom.sh still carries the digests this mirror expects.

# Mirror of install-loom.sh's LOOM_RETIRED_FILES allowlist (#3572). Keep in
# sync with install-loom.sh — assert_retired_allowlist_in_sync guards drift.
RETIRED_ALLOWLIST_MIRROR=$(cat <<'RETIRED'
.claude/commands/loom/release.md 11aef217942f45bd03d90a24e5efae9209041cb59f09c888df4dc7e8208910dd
.claude/commands/loom/release.md 0df6c20846c98850413243362c80dea2fd01330c8d97033ef5f7c3989578fe8c
.claude/commands/loom/release.md c45841f8da42d1bda20bc180c8a93d14242238d9a2c1d9f5a1bdac32b5e9e556
.claude/commands/loom/release.md d91e198e977ad7799f44fa1a6827c9836bca6d31c9357ed92fc400a3c88381de
.claude/commands/loom/release.md 0d7030dd14f32f6f382a6430cd04e5f0475825d567aaed7570b73a4c43128ad1
.claude/commands/loom/release.md 4a077ed25cb44add0afbc4d6bda23cb372f5f3c4c2ef23b7a24b586e66e4f3e7
.claude/commands/loom/release.md 5f9930dc72a263866122b18018a64b8fed4bd53ef623d0eef27ed1e31fa0502f
.claude/commands/loom/release.md b7fae9d13d2bfaee3bde514cabe44ac70b6551351a9e49357ede00f82c17cf35
.claude/commands/loom/release.md f6523d9be058e40397f0ce30c08a8f2b60e9b38adae04bd7c919e0cc840acfec
.claude/commands/loom/release.md 29a845f7f8912545d23832551753304df6e72dd4a9c8082c2d8ada1f09f449e1
.claude/commands/loom/release.md 795c1df1d3f3706ba448482b037a0c9e4eb6272a719adb2688b9ddfc91ab4de6
RETIRED
)

# The git blob sha of the last release.md version Loom shipped (parent of the
# #3571 deletion). Immutable + content-addressed; its sha256 is the first row
# of the mirror above. Used to reconstruct real shipped bytes at test time.
RETIRED_LAST_SHIPPED_BLOB="b1dac86f43dbe159b1a617b31010cdaab7b88bc5"
RETIRED_RELEASE_PATH=".claude/commands/loom/release.md"

_test_sha256() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }

# Mirror of the install-loom.sh gate: prints REMOVE / PRESERVE / NONE for the
# retired path under repo $1.
retired_decision() {
  local repo="$1" rp="$RETIRED_RELEASE_PATH"
  [[ -f "$repo/$rp" ]] || { echo "NONE"; return 0; }
  local fh; fh="$(_test_sha256 "$repo/$rp")"
  local matched=false ap ah
  if [[ -n "$fh" ]]; then
    while read -r ap ah; do
      [[ -n "$ap" && "${ap:0:1}" != "#" ]] || continue
      if [[ "$ap" == "$rp" && "$ah" == "$fh" ]]; then matched=true; break; fi
    done <<< "$RETIRED_ALLOWLIST_MIRROR"
  fi
  if [[ "$matched" == "true" ]]; then echo "REMOVE"; else echo "PRESERVE"; fi
}

# Drift guard: every digest in the test mirror must be present in the real
# install-loom.sh allowlist (and vice-versa for the release.md rows).
assert_retired_allowlist_in_sync() {
  local ok=true ap ah
  while read -r ap ah; do
    [[ -n "$ap" && "${ap:0:1}" != "#" ]] || continue
    if ! grep -qF "$ap $ah" "$SCRIPT_DIR/install-loom.sh"; then
      ok=false
      warn "mirror digest missing from install-loom.sh: $ap $ah"
    fi
  done <<< "$RETIRED_ALLOWLIST_MIRROR"
  if [[ "$ok" == "true" ]]; then
    pass "Retired-file allowlist in test mirror matches install-loom.sh"
  else
    fail "Retired-file allowlist drifted between test mirror and install-loom.sh"
  fi
}

echo "--- Section 5b: Retired-File Cleanup (#3572) ---"
echo ""

# Test 44a: an unmodified (hash-matching) retired file is removed on update.
echo "Test 44a: Unmodified retired release.md is removed"
RETIRED_REMOVE_REPO="$TEST_DIR/retired-remove-test"
create_temp_repo "$RETIRED_REMOVE_REPO"
mkdir -p "$RETIRED_REMOVE_REPO/$(dirname "$RETIRED_RELEASE_PATH")"
if git -C "$LOOM_ROOT" cat-file -e "$RETIRED_LAST_SHIPPED_BLOB" 2>/dev/null; then
  git -C "$LOOM_ROOT" cat-file blob "$RETIRED_LAST_SHIPPED_BLOB" \
    > "$RETIRED_REMOVE_REPO/$RETIRED_RELEASE_PATH"
  git -C "$RETIRED_REMOVE_REPO" add "$RETIRED_RELEASE_PATH"
  git -C "$RETIRED_REMOVE_REPO" commit -m "Add shipped release.md" --quiet

  # The reconstructed bytes must hash to the head of the allowlist — this is
  # the real linkage between "what Loom shipped" and "what the gate removes".
  SHIPPED_HASH="$(_test_sha256 "$RETIRED_REMOVE_REPO/$RETIRED_RELEASE_PATH")"
  if grep -qF "$RETIRED_RELEASE_PATH $SHIPPED_HASH" "$SCRIPT_DIR/install-loom.sh"; then
    pass "Reconstructed shipped release.md hash is in install-loom.sh allowlist"
  else
    fail "Shipped release.md hash ($SHIPPED_HASH) absent from install-loom.sh allowlist"
  fi

  if [[ "$(retired_decision "$RETIRED_REMOVE_REPO")" == "REMOVE" ]]; then
    pass "Unmodified release.md gated for removal"
  else
    fail "Unmodified release.md not gated for removal"
  fi
  # Apply the sweep (mirrors install-loom.sh git-rm step) and verify removal.
  git -C "$RETIRED_REMOVE_REPO" rm --quiet --force "$RETIRED_RELEASE_PATH" 2>/dev/null || true
  if [[ ! -f "$RETIRED_REMOVE_REPO/$RETIRED_RELEASE_PATH" ]]; then
    pass "Unmodified release.md removed from working tree"
  else
    fail "Unmodified release.md still present after cleanup"
  fi

  # Test 44d: idempotency — a second run with the file already gone is a no-op.
  echo ""
  echo "Test 44d: Cleanup is idempotent (second run is a no-op)"
  if [[ "$(retired_decision "$RETIRED_REMOVE_REPO")" == "NONE" ]]; then
    pass "Second cleanup run is a no-op (file already absent)"
  else
    fail "Second cleanup run did not treat absent file as no-op"
  fi
else
  warn "Skipping Test 44a/44d: shipped release.md blob $RETIRED_LAST_SHIPPED_BLOB unreachable (shallow clone?)"
fi
echo ""

# Test 44b: a consumer-modified retired file (hash matches none) is preserved.
echo "Test 44b: Consumer-modified release.md is preserved"
RETIRED_KEEP_REPO="$TEST_DIR/retired-keep-test"
create_temp_repo "$RETIRED_KEEP_REPO"
mkdir -p "$RETIRED_KEEP_REPO/$(dirname "$RETIRED_RELEASE_PATH")"
printf '# my customized release skill\nlocal edits here\n' \
  > "$RETIRED_KEEP_REPO/$RETIRED_RELEASE_PATH"
git -C "$RETIRED_KEEP_REPO" add "$RETIRED_RELEASE_PATH"
git -C "$RETIRED_KEEP_REPO" commit -m "Add customized release.md" --quiet
if [[ "$(retired_decision "$RETIRED_KEEP_REPO")" == "PRESERVE" ]]; then
  pass "Consumer-modified release.md gated for preservation"
else
  fail "Consumer-modified release.md not preserved (hash matched allowlist unexpectedly)"
fi
if [[ -f "$RETIRED_KEEP_REPO/$RETIRED_RELEASE_PATH" ]]; then
  pass "Consumer-modified release.md left on disk"
else
  fail "Consumer-modified release.md was removed (should be preserved)"
fi
echo ""

# Test 44c: absent retired file is a no-op (no error, no removal).
echo "Test 44c: Absent release.md is a no-op"
RETIRED_ABSENT_REPO="$TEST_DIR/retired-absent-test"
create_temp_repo "$RETIRED_ABSENT_REPO"
if [[ "$(retired_decision "$RETIRED_ABSENT_REPO")" == "NONE" ]]; then
  pass "Absent release.md yields no cleanup action"
else
  fail "Absent release.md did not yield a no-op"
fi
echo ""

# Drift guard: the test mirror's digests must match install-loom.sh.
assert_retired_allowlist_in_sync
echo ""


# ==========================================================================
# Section 8: Flag Rejection Tests (#3423 acceptance criteria)
# ==========================================================================
# The unknown-flag guard in install-loom.sh (lines ~120-124) fires before any
# path validation, so a non-existent path is fine for these tests.
echo "--- Section 8: Flag Rejection ---"
echo ""

# Test 45: --quick is rejected with an actionable error message
# Note: set -e is active; capture stderr + suppress non-zero exit via || true.
echo "Test 45: install-loom.sh --quick is rejected with actionable error"
STDERR_45=$("$INSTALL_SCRIPT" --quick /tmp/fakepath 2>&1 >/dev/null || true)
if [[ -n "$STDERR_45" ]] && echo "$STDERR_45" | grep -q 'unknown flag: --quick'; then
  pass "--quick rejected with correct error message"
else
  fail "--quick should be rejected with 'Error: unknown flag: --quick' (stderr=$STDERR_45)"
fi
echo ""

# Test 46: --foo (arbitrary unknown flag) is rejected with an actionable error message
echo "Test 46: install-loom.sh --foo is rejected with actionable error"
STDERR_46=$("$INSTALL_SCRIPT" --foo /tmp/fakepath 2>&1 >/dev/null || true)
if [[ -n "$STDERR_46" ]] && echo "$STDERR_46" | grep -q 'unknown flag: --foo'; then
  pass "--foo rejected with correct error message"
else
  fail "--foo should be rejected with 'Error: unknown flag: --foo' (stderr=$STDERR_46)"
fi
echo ""

# Test 47: hint text references install.sh so the operator knows where --quick/--full belong
echo "Test 47: flag-rejection error mentions install.sh as the correct entry point"
if echo "$STDERR_45" | grep -q 'install\.sh'; then
  pass "flag-rejection stderr contains 'install.sh' hint text"
else
  fail "flag-rejection stderr is missing 'install.sh' hint (stderr=$STDERR_45)"
fi
echo ""

# ==========================================================================
# Section 8b: Wrapper Pass-Through Flags (#3650)
# ==========================================================================
# The top-level install.sh wrapper previously rejected --allow-non-main-source
# and --allow-stale-target with "Unknown flag" even though it suggested the
# former in its own delegated installer, and its delegation execs forwarded
# only --yes/$FORCE_FLAG. These tests verify the wrapper now accepts and
# forwards the two source/target override flags that scripts/install-loom.sh
# already honors.
echo "--- Section 8b: Wrapper Pass-Through Flags (#3650) ---"
echo ""

# Test 48: install.sh --allow-non-main-source is NOT rejected as an unknown flag.
# Trailing --help makes the parser exit 0 after accumulating the pass-through
# flag, so no real install runs. A rejected flag would error before --help.
echo "Test 48: install.sh accepts --allow-non-main-source (no 'Unknown flag')"
OUT_48=$("$WRAPPER_SCRIPT" --allow-non-main-source --help 2>&1 || true)
if echo "$OUT_48" | grep -q 'Unknown flag'; then
  fail "install.sh rejected --allow-non-main-source (out=$OUT_48)"
elif echo "$OUT_48" | grep -q 'Usage:'; then
  pass "--allow-non-main-source accepted (parser reached --help)"
else
  fail "install.sh --allow-non-main-source produced unexpected output (out=$OUT_48)"
fi
echo ""

# Test 49: install.sh --allow-stale-target is likewise accepted.
echo "Test 49: install.sh accepts --allow-stale-target (no 'Unknown flag')"
OUT_49=$("$WRAPPER_SCRIPT" --allow-stale-target --help 2>&1 || true)
if echo "$OUT_49" | grep -q 'Unknown flag'; then
  fail "install.sh rejected --allow-stale-target (out=$OUT_49)"
elif echo "$OUT_49" | grep -q 'Usage:'; then
  pass "--allow-stale-target accepted (parser reached --help)"
else
  fail "install.sh --allow-stale-target produced unexpected output (out=$OUT_49)"
fi
echo ""

# Test 50: a genuinely unknown flag is still rejected by install.sh.
echo "Test 50: install.sh still rejects a genuinely unknown flag"
OUT_50=$("$WRAPPER_SCRIPT" --bogus /tmp/fakepath 2>&1 || true)
if echo "$OUT_50" | grep -q 'Unknown flag: --bogus'; then
  pass "--bogus rejected with 'Unknown flag: --bogus'"
else
  fail "install.sh should reject --bogus with 'Unknown flag' (out=$OUT_50)"
fi
echo ""

# Test 51: install.sh --help documents the two pass-through flags.
echo "Test 51: install.sh --help lists the pass-through flags"
OUT_51=$("$WRAPPER_SCRIPT" --help 2>&1 || true)
if echo "$OUT_51" | grep -q -- '--allow-non-main-source' && echo "$OUT_51" | grep -q -- '--allow-stale-target'; then
  pass "--help documents --allow-non-main-source and --allow-stale-target"
else
  fail "install.sh --help is missing pass-through flag documentation (out=$OUT_51)"
fi
echo ""

# Test 52: both Full-Install delegation execs forward the pass-through array so
# the accepted flags actually reach scripts/install-loom.sh.
echo "Test 52: install.sh forwards SOURCE_OVERRIDE_FLAGS at both delegation execs"
FORWARD_COUNT=$(grep -c 'install-loom.sh".*SOURCE_OVERRIDE_FLAGS\[@\]' "$WRAPPER_SCRIPT" || true)
if [[ "$FORWARD_COUNT" -eq 2 ]]; then
  pass "both delegation execs forward SOURCE_OVERRIDE_FLAGS (count=$FORWARD_COUNT)"
else
  fail "expected 2 delegation execs forwarding SOURCE_OVERRIDE_FLAGS, found $FORWARD_COUNT"
fi
echo ""


# ==========================================================================
# Section 9: Consumer-File Preservation Across Reinstall (#3450)
# ==========================================================================
# Regression tests for issue #3450: install.sh --quick --yes on a v0.7.2
# Loom install destroyed three sets of consumer-owned files:
#   1. CLAUDE.md — 1011-line consumer file rewritten to 2 lines
#   2. .gitignore — 296 lines truncated to ~38 (Loom-only)
#   3. .github/workflows/{ci,deploy}.yml + .github/ISSUE_TEMPLATE/agent-submission.yml — deleted
#
# Root cause: scripts/install-loom.sh's installed_files manifest used
# `find .loom .claude .codex .github .githooks CLAUDE.md .gitignore` and
# captured every file under those roots — INCLUDING consumer-authored files
# that Loom never installed. scripts/uninstall-loom.sh then hard-deleted
# every manifest entry (except CLAUDE.md / .claude/settings.json) and the
# CLAUDE.md substring-match branch fired on any file mentioning the legacy
# Loom signature phrases.
#
# These tests simulate the v0.7.2-shape over-broad manifest and verify that
# the uninstall path now preserves consumer-owned content end-to-end.

echo "--- Section 9: Consumer-File Preservation Across Reinstall (#3450) ---"
echo ""

# Helper: write an "over-broad" install-metadata.json that lists consumer files.
# This is the shape that v0.7.2's scripts/install-loom.sh produced.
write_overbroad_manifest() {
  local target="$1"
  shift
  local files=("$@")

  local json="["
  local first=true
  for f in "${files[@]}"; do
    if [[ "$first" == "true" ]]; then
      first=false
    else
      json="${json},"
    fi
    json="${json}\"${f}\""
  done
  json="${json}]"

  mkdir -p "$target/.loom"
  cat > "$target/.loom/install-metadata.json" <<META_EOF
{
  "loom_version": "0.7.2",
  "loom_commit": "v072test",
  "install_date": "2025-01-01",
  "loom_source": "$LOOM_ROOT",
  "installed_files": ${json}
}
META_EOF
}

# Test 48: .gitignore consumer content survives uninstall (AC2)
# Even when v0.7.2-shape manifest lists .gitignore as Loom-installed, the
# uninstall must route it through smart-removal — never hard-delete.
echo "Test 48: .gitignore consumer content survives uninstall (v0.7.2 manifest)"
GI_REPO="$TEST_DIR/gitignore-preserve-test"
create_temp_repo "$GI_REPO"
simulate_loom_install "$GI_REPO"

# Replace .gitignore with consumer content + Loom patterns (mimics real-world
# v0.7.2 user state where the consumer's pre-existing .gitignore was extended
# by the installer with Loom runtime patterns).
cat > "$GI_REPO/.gitignore" <<'GI_EOF'
# Consumer ignore rules (must survive uninstall)
node_modules/
target/
__pycache__/
.venv/
*.log
.idea/
.DS_Store
dist/
build/
coverage/
.env
.env.local

# Loom - AI Development Orchestration
.loom/state.json
.loom/worktrees/
.loom/*.log
.loom/*.sock
GI_EOF

# Over-broad manifest lists .gitignore as Loom-owned (the v0.7.2 bug).
write_overbroad_manifest "$GI_REPO" \
  ".gitignore" \
  ".loom/config.json" \
  ".loom/roles/builder.json"

git -C "$GI_REPO" add -A
git -C "$GI_REPO" commit -m "v0.7.2 user state" --quiet

GI_LINES_BEFORE=$(wc -l < "$GI_REPO/.gitignore" | tr -d ' ')

"$UNINSTALL_SCRIPT" --yes --local "$GI_REPO" > /dev/null 2>&1 || true

if [[ -f "$GI_REPO/.gitignore" ]]; then
  if grep -q "node_modules/" "$GI_REPO/.gitignore" && \
     grep -q "__pycache__/" "$GI_REPO/.gitignore" && \
     grep -q ".venv/" "$GI_REPO/.gitignore"; then
    GI_LINES_AFTER=$(wc -l < "$GI_REPO/.gitignore" | tr -d ' ')
    pass ".gitignore consumer content preserved ($GI_LINES_BEFORE -> $GI_LINES_AFTER lines)"
  else
    fail ".gitignore consumer content was destroyed (lines: $(wc -l < "$GI_REPO/.gitignore" | tr -d ' '))"
  fi
else
  fail ".gitignore was hard-deleted by uninstall (v0.7.2 manifest path)"
fi

# Verify Loom-specific patterns were smart-removed
if grep -q "Loom - AI Development Orchestration" "$GI_REPO/.gitignore" 2>/dev/null; then
  fail ".gitignore Loom marker header should have been removed"
else
  pass ".gitignore Loom marker header was smart-removed"
fi
echo ""

# Test 49: CLAUDE.md consumer content survives uninstall when consumer text
# mentions the legacy Loom signature substrings (AC1).
# Reproduces the 1011-line -> 2-line CLAUDE.md destruction reported in #3450.
echo "Test 49: CLAUDE.md with consumer content mentioning Loom is preserved"
CMD_REPO="$TEST_DIR/claudemd-mentions-loom-test"
create_temp_repo "$CMD_REPO"
simulate_loom_install "$CMD_REPO"

# Write a multi-hundred-line consumer CLAUDE.md whose content happens to
# mention the legacy substrings (in code blocks, headings, changelog).
# This is the file shape that triggered the v0.7.2 bug.
{
  echo "# My Project Guide"
  echo ""
  echo "This is the consumer-authored project guide. It must NOT be deleted."
  echo ""
  echo "## Changelog"
  echo ""
  echo "- v1.0: Initial release"
  echo "- v1.1: We migrated from a system whose docs mentioned"
  echo '  "# Loom Orchestration - Repository Guide" in a heading.'
  echo "- v1.2: Updated installer docs reference 'Generated by Loom Installation Process'"
  echo "  in a code block:"
  echo ""
  echo '  ```'
  echo "  Generated by Loom Installation Process"
  echo '  ```'
  echo ""
  echo "## Architecture"
  echo ""
  for i in $(seq 1 200); do
    echo "Project documentation paragraph $i — consumer-owned content."
  done
  echo ""
  echo "<!-- BEGIN LOOM ORCHESTRATION -->"
  echo "This repository uses Loom for AI-powered development orchestration."
  echo "<!-- END LOOM ORCHESTRATION -->"
  echo ""
  echo "## Closing Notes"
  echo ""
  echo "More consumer content after the Loom block — must also survive."
} > "$CMD_REPO/CLAUDE.md"

git -C "$CMD_REPO" add -A
git -C "$CMD_REPO" commit -m "v0.7.2 CLAUDE.md user state" --quiet

CMD_LINES_BEFORE=$(wc -l < "$CMD_REPO/CLAUDE.md" | tr -d ' ')

"$UNINSTALL_SCRIPT" --yes --local "$CMD_REPO" > /dev/null 2>&1 || true

if [[ -f "$CMD_REPO/CLAUDE.md" ]]; then
  CMD_LINES_AFTER=$(wc -l < "$CMD_REPO/CLAUDE.md" | tr -d ' ')
  if grep -q "My Project Guide" "$CMD_REPO/CLAUDE.md" && \
     grep -q "consumer-owned content" "$CMD_REPO/CLAUDE.md" && \
     grep -q "Closing Notes" "$CMD_REPO/CLAUDE.md"; then
    pass "CLAUDE.md consumer content preserved ($CMD_LINES_BEFORE -> $CMD_LINES_AFTER lines)"
  else
    fail "CLAUDE.md consumer content was destroyed (lines: $CMD_LINES_AFTER, originally $CMD_LINES_BEFORE)"
  fi
  # The Loom marker block should be removed
  if grep -q "BEGIN LOOM ORCHESTRATION" "$CMD_REPO/CLAUDE.md"; then
    fail "CLAUDE.md still contains Loom marker block (should be removed)"
  else
    pass "CLAUDE.md Loom marker block was removed"
  fi
else
  fail "CLAUDE.md was deleted entirely (v0.7.2 substring heuristic bug)"
fi
echo ""

# Test 49b: CLAUDE.md without markers but with legacy signatures in consumer
# content survives uninstall. This reproduces the 1011-line -> 2-line
# destruction reported in #3450 when the v0.7.2 marker shape didn't match
# the modern sed pattern and the substring heuristic fired on consumer text.
echo "Test 49b: CLAUDE.md without markers but mentioning Loom is preserved"
CMD2_REPO="$TEST_DIR/claudemd-no-markers-test"
create_temp_repo "$CMD2_REPO"
simulate_loom_install "$CMD2_REPO"

# Write a multi-hundred-line consumer CLAUDE.md WITHOUT modern markers.
# Consumer text mentions "Generated by Loom Installation Process" in a
# changelog code block — exactly the kind of mention that the substring
# heuristic conflates with "this file IS Loom-generated".
{
  echo "# Consumer Project Guide"
  echo ""
  echo "Comprehensive consumer-authored documentation. Must NOT be deleted."
  echo ""
  echo "## Migration history"
  echo ""
  echo "Previously this repo was managed by an installer that wrote:"
  echo ""
  echo '```'
  echo "Generated by Loom Installation Process"
  echo '```'
  echo ""
  echo "as a footer. We've since written our own docs."
  echo ""
  for i in $(seq 1 300); do
    echo "Section $i: detailed consumer-owned guidance and architecture notes."
  done
} > "$CMD2_REPO/CLAUDE.md"

git -C "$CMD2_REPO" add -A
git -C "$CMD2_REPO" commit -m "Consumer CLAUDE.md with no markers" --quiet

CMD2_LINES_BEFORE=$(wc -l < "$CMD2_REPO/CLAUDE.md" | tr -d ' ')

"$UNINSTALL_SCRIPT" --yes --local "$CMD2_REPO" > /dev/null 2>&1 || true

if [[ -f "$CMD2_REPO/CLAUDE.md" ]]; then
  CMD2_LINES_AFTER=$(wc -l < "$CMD2_REPO/CLAUDE.md" | tr -d ' ')
  if grep -q "Consumer Project Guide" "$CMD2_REPO/CLAUDE.md" && \
     grep -q "Section 300" "$CMD2_REPO/CLAUDE.md"; then
    pass "CLAUDE.md (no-markers) consumer content preserved ($CMD2_LINES_BEFORE -> $CMD2_LINES_AFTER lines)"
  else
    fail "CLAUDE.md (no-markers) consumer content truncated (lines: $CMD2_LINES_AFTER, originally $CMD2_LINES_BEFORE)"
  fi
else
  fail "CLAUDE.md (no-markers) was deleted entirely by substring heuristic (#3450 bug)"
fi
echo ""

# Test 50: .github/workflows/* consumer files survive uninstall (AC3)
# When the v0.7.2 manifest listed consumer-authored workflow files, the
# uninstall hard-delete loop wiped them. The narrowed manifest (Fix 1) plus
# the inert uninstall path mean these survive.
echo "Test 50: .github/workflows/* consumer files survive (v0.7.2 manifest)"
GH_REPO_DIR="$TEST_DIR/github-workflows-preserve-test"
create_temp_repo "$GH_REPO_DIR"
simulate_loom_install "$GH_REPO_DIR"

# Create consumer-authored workflow + issue template files
mkdir -p "$GH_REPO_DIR/.github/workflows"
mkdir -p "$GH_REPO_DIR/.github/ISSUE_TEMPLATE"
cat > "$GH_REPO_DIR/.github/workflows/ci.yml" <<'WF_EOF'
name: CI
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: echo "consumer CI"
WF_EOF
cat > "$GH_REPO_DIR/.github/workflows/deploy.yml" <<'WF_EOF'
name: Deploy
on:
  push:
    branches: [main]
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - run: echo "consumer deploy"
WF_EOF
cat > "$GH_REPO_DIR/.github/ISSUE_TEMPLATE/agent-submission.yml" <<'IT_EOF'
name: Agent Submission
description: Consumer-owned issue template
body:
  - type: textarea
    id: details
    attributes:
      label: Details
IT_EOF

# Over-broad manifest: includes consumer-authored .github files (the v0.7.2 bug).
write_overbroad_manifest "$GH_REPO_DIR" \
  ".github/labels.yml" \
  ".github/ISSUE_TEMPLATE/config.yml" \
  ".github/ISSUE_TEMPLATE/task.yml" \
  ".github/workflows/ci.yml" \
  ".github/workflows/deploy.yml" \
  ".github/ISSUE_TEMPLATE/agent-submission.yml" \
  ".loom/config.json"

git -C "$GH_REPO_DIR" add -A
git -C "$GH_REPO_DIR" commit -m "v0.7.2 user state with consumer workflows" --quiet

"$UNINSTALL_SCRIPT" --yes --local "$GH_REPO_DIR" > /dev/null 2>&1 || true

if [[ -f "$GH_REPO_DIR/.github/workflows/ci.yml" ]]; then
  pass ".github/workflows/ci.yml preserved across uninstall"
else
  fail ".github/workflows/ci.yml was deleted (v0.7.2 manifest bug)"
fi

if [[ -f "$GH_REPO_DIR/.github/workflows/deploy.yml" ]]; then
  pass ".github/workflows/deploy.yml preserved across uninstall"
else
  fail ".github/workflows/deploy.yml was deleted (v0.7.2 manifest bug)"
fi

if [[ -f "$GH_REPO_DIR/.github/ISSUE_TEMPLATE/agent-submission.yml" ]]; then
  pass ".github/ISSUE_TEMPLATE/agent-submission.yml preserved across uninstall"
else
  fail ".github/ISSUE_TEMPLATE/agent-submission.yml was deleted (v0.7.2 manifest bug)"
fi
echo ""

# Test 51: Manifest narrowing — fresh install-loom.sh produces a manifest
# whose installed_files list contains ONLY files shipped under defaults/.
# Specifically, .gitignore must NOT be in the manifest (smart-removal owns it).
echo "Test 51: Fresh install manifest only lists files shipped in defaults/"
# Simulate what the narrowed install-loom.sh produces by exercising the
# helper directly. We don't need to run the full installer — the manifest
# narrowing logic is what we're testing.
NARROW_REPO="$TEST_DIR/narrow-manifest-test"
create_temp_repo "$NARROW_REPO"
simulate_loom_install "$NARROW_REPO"

# Add some consumer-authored files OUTSIDE Loom's defaults/ footprint.
mkdir -p "$NARROW_REPO/.github/workflows"
echo "name: ConsumerCI" > "$NARROW_REPO/.github/workflows/ci.yml"
cat >> "$NARROW_REPO/.gitignore" <<'EOF'
# Consumer additions
my-secrets/
EOF
git -C "$NARROW_REPO" add -A
git -C "$NARROW_REPO" commit -m "consumer additions" --quiet 2>/dev/null || true

# The manifest in simulate_loom_install was written by the same over-broad
# find. We assert what the *narrowed* shell helper would emit. The helper
# lives in scripts/install/manifest.sh (sourced by install-loom.sh).
MANIFEST_LIB="$LOOM_ROOT/scripts/install/manifest.sh"
if [[ -f "$MANIFEST_LIB" ]]; then
  NARROW_JSON=$(
    # shellcheck disable=SC1090
    source "$MANIFEST_LIB"
    LOOM_ROOT="$LOOM_ROOT" TARGET_PATH="$NARROW_REPO" _emit_installed_files_manifest 2>/dev/null
  )

  # AC3 narrowing: the manifest must NOT list the consumer's .github/workflows/ci.yml.
  if echo "$NARROW_JSON" | grep -qF '".github/workflows/ci.yml"'; then
    fail "Narrowed manifest still lists consumer-owned .github/workflows/ci.yml"
  else
    pass "Narrowed manifest excludes consumer-owned .github/workflows/ci.yml"
  fi

  # AC2 narrowing: the manifest must NOT list .gitignore (smart-removal owns it).
  if echo "$NARROW_JSON" | grep -qF '".gitignore"'; then
    fail "Narrowed manifest still lists .gitignore (must be smart-removed only)"
  else
    pass "Narrowed manifest excludes .gitignore (handled by smart-removal)"
  fi

  # Positive check: the manifest must list at least one file Loom actually ships.
  if echo "$NARROW_JSON" | grep -qF '".loom/config.json"'; then
    pass "Narrowed manifest includes Loom-shipped .loom/config.json"
  else
    fail "Narrowed manifest is missing Loom-shipped .loom/config.json"
  fi

  # Positive check: roles are translated correctly (defaults/roles/X → .loom/roles/X)
  if echo "$NARROW_JSON" | grep -qF '".loom/roles/builder.json"'; then
    pass "Narrowed manifest translates defaults/roles/* → .loom/roles/*"
  else
    fail "Narrowed manifest missing translated defaults/roles/* entries"
  fi
else
  fail "scripts/install/manifest.sh is missing"
fi
echo ""


# Test 52: Loom-internal skills are not shipped to consumer repos (#3464)
# After `loom-daemon init` against a fresh consumer repo, the entries
# listed in defaults/.loom-internal.list must NOT exist in the consumer
# tree, while sibling commands (builder, judge, curator) must exist.
#
# This test runs the real `loom-daemon init` (the same call install.sh
# makes) rather than `simulate_loom_install`, because the leakage fix
# lives inside `loom-daemon::init::scaffolding::setup_repository_scaffolding`
# and the simulator's `cp -r .claude` does not exercise the skip path.
echo "Test 52: Loom-internal skills excluded from consumer install (#3464)"
DAEMON_BIN_52="$LOOM_ROOT/target/release/loom-daemon"
SKIP_LIST_FILE="$LOOM_ROOT/defaults/.loom-internal.list"
if [[ ! -x "$DAEMON_BIN_52" ]]; then
  warn "Skipping Test 52 — loom-daemon release binary not built at $DAEMON_BIN_52"
elif [[ ! -f "$SKIP_LIST_FILE" ]]; then
  fail "defaults/.loom-internal.list is missing — the skip mechanism requires this file"
else
  INTERNAL_REPO="$TEST_DIR/internal-skip-test"
  create_temp_repo "$INTERNAL_REPO"

  # `loom-daemon init` builds a real consumer install in INTERNAL_REPO.
  # Suppress stdout — we only care about side effects on the filesystem.
  if "$DAEMON_BIN_52" init --defaults "$LOOM_ROOT/defaults" "$INTERNAL_REPO" >/dev/null 2>&1; then
    # Each listed defaults-relative path must NOT exist in the consumer.
    skip_violations=0
    while IFS= read -r skip_rel; do
      # Strip comments and blank lines (mirror the skip-list reader).
      skip_rel="${skip_rel%%#*}"
      # shellcheck disable=SC2295
      skip_rel="${skip_rel#"${skip_rel%%[![:space:]]*}"}"
      skip_rel="${skip_rel%"${skip_rel##*[![:space:]]}"}"
      [[ -z "$skip_rel" ]] && continue
      if [[ -e "$INTERNAL_REPO/$skip_rel" ]]; then
        fail "Loom-internal file leaked to consumer: $skip_rel"
        skip_violations=$((skip_violations + 1))
      fi
    done < "$SKIP_LIST_FILE"
    if [[ "$skip_violations" -eq 0 ]]; then
      pass "All defaults/.loom-internal.list entries absent from consumer tree"
    fi

    # Issue #3563: the /loom:release skill was retired in favor of
    # /repo:release (rjwalters/repo). Loom no longer ships release.md; pin its
    # absence so a future regression that re-adds it fails this test.
    if [[ ! -f "$INTERNAL_REPO/.claude/commands/loom/release.md" ]]; then
      pass "#3563: retired .claude/commands/loom/release.md does not ship to consumers"
    else
      fail "#3563: .claude/commands/loom/release.md should not be installed (skill retired)"
    fi

    # The siblings must continue to ship — pin three representative skills.
    sibling_ok=true
    for sibling in builder.md judge.md curator.md; do
      if [[ ! -f "$INTERNAL_REPO/.claude/commands/loom/$sibling" ]]; then
        fail "Consumer install missing .claude/commands/loom/$sibling"
        sibling_ok=false
      fi
    done
    if $sibling_ok; then
      pass "Consumer install includes builder.md, judge.md, curator.md (skip-list is narrow)"
    fi

    # #3468 AC1: the new generic /loom:bump skill must ship to consumers.
    # (It is the lightweight quick-bump; full releases use /repo:release.)
    if [[ -f "$INTERNAL_REPO/.claude/commands/loom/bump.md" ]]; then
      pass "AC1 (#3468): /loom:bump skill ships to consumers"
    else
      fail "AC1 (#3468): .claude/commands/loom/bump.md missing from consumer install"
    fi
  else
    fail "loom-daemon init failed against fresh consumer repo $INTERNAL_REPO"
  fi
fi
echo ""


# ==========================================================================
# Section 10: Ownership-Boundary Intersection (#3492)
# ==========================================================================
# Regression tests for issue #3492: pre-#3450 installs persisted an
# over-broad on-disk manifest under .loom/install-metadata.json that
# captured consumer-authored files outside Loom's ownership boundary
# (e.g. .claude/skills/anvil-memo/SKILL.md, .claude/commands/<non-loom>/).
# The fix intersects every deletion candidate against the CURRENT
# Loom ownership set produced by _emit_loom_ownership_set; paths the
# previous manifest claimed Loom owned but that the current defaults/
# does not ship are preserved with a warning, never deleted.
#
# These tests cover both deletion call sites:
#  • Test 53 — install-loom.sh upgrade stale-file sweep
#  • Test 54 — uninstall-loom.sh hard-delete loop (--yes --local)

echo "--- Section 10: Ownership-Boundary Intersection (#3492) ---"
echo ""

# Test 53: Stale-file sweep preserves files not in current ownership set.
# Mirrors install-loom.sh's stale-file sweep — the upgrade path — and
# asserts that .claude/skills/anvil-memo/SKILL.md (a path Loom never
# ships) survives even when an over-broad legacy manifest lists it.
echo "Test 53: Stale-file sweep preserves files outside current ownership set"
OWNERSHIP_SWEEP_REPO="$TEST_DIR/ownership-sweep-test"
create_temp_repo "$OWNERSHIP_SWEEP_REPO"
simulate_loom_install "$OWNERSHIP_SWEEP_REPO"

# Consumer-authored files captured by an over-broad pre-#3450 manifest.
# Multiple paths to confirm the gate is per-file, not per-prefix.
CONSUMER_SKILL=".claude/skills/anvil-memo/SKILL.md"
CONSUMER_COMMAND=".claude/commands/repo/lint.md"
CONSUMER_HOOK=".claude/hooks/project-specific.sh"

mkdir -p "$OWNERSHIP_SWEEP_REPO/.claude/skills/anvil-memo"
mkdir -p "$OWNERSHIP_SWEEP_REPO/.claude/commands/repo"
mkdir -p "$OWNERSHIP_SWEEP_REPO/.claude/hooks"
echo "# Anvil memo skill" > "$OWNERSHIP_SWEEP_REPO/$CONSUMER_SKILL"
echo "# Lint command" > "$OWNERSHIP_SWEEP_REPO/$CONSUMER_COMMAND"
echo "#!/bin/bash" > "$OWNERSHIP_SWEEP_REPO/$CONSUMER_HOOK"
git -C "$OWNERSHIP_SWEEP_REPO" add -A
git -C "$OWNERSHIP_SWEEP_REPO" commit -m "consumer files outside Loom boundary" --quiet

# Over-broad manifest lists the consumer files alongside a real Loom file.
# Simulates what a pre-#3450 install-metadata.json would contain.
cat > "$OWNERSHIP_SWEEP_REPO/.loom/install-metadata.json" <<EOF
{
  "loom_version": "0.7.1",
  "loom_commit": "old",
  "install_date": "2026-01-01",
  "loom_source": "$LOOM_ROOT",
  "installed_files": ["$CONSUMER_SKILL","$CONSUMER_COMMAND","$CONSUMER_HOOK",".loom/scripts/old-stale.sh"]
}
EOF
# A genuine Loom-shipped stale file (used to be in defaults/, now removed).
echo "#!/bin/bash" > "$OWNERSHIP_SWEEP_REPO/.loom/scripts/old-stale.sh"
git -C "$OWNERSHIP_SWEEP_REPO" add .loom/scripts/old-stale.sh
git -C "$OWNERSHIP_SWEEP_REPO" commit -m "Loom-shipped stale file" --quiet

# Run a real install via install-loom.sh.  We're not exercising the curator
# / PR flow here — pass --yes --local-only via the env vars install-loom.sh
# honors.  Simpler: directly compute the ownership boundary against
# install-loom.sh's sweep logic by sourcing manifest.sh and replaying the
# intersect check.
# shellcheck disable=SC1090
source "$LOOM_ROOT/scripts/install/manifest.sh"
OWNERSHIP_SET="$(LOOM_ROOT="$LOOM_ROOT" TARGET_PATH="$OWNERSHIP_SWEEP_REPO" _emit_loom_ownership_set)"

# The ownership set MUST include the genuine Loom-shipped path (canary).
if printf '%s\n' "$OWNERSHIP_SET" | grep -Fxq -- ".loom/scripts/check-host-sleep.sh"; then
  pass "Ownership set includes a Loom-shipped script (.loom/scripts/check-host-sleep.sh)"
else
  fail "Ownership set missing canary .loom/scripts/check-host-sleep.sh"
fi

# The ownership set MUST NOT include consumer-authored paths.
OWNERSHIP_OK=true
for consumer_file in "$CONSUMER_SKILL" "$CONSUMER_COMMAND" "$CONSUMER_HOOK"; do
  if printf '%s\n' "$OWNERSHIP_SET" | grep -Fxq -- "$consumer_file"; then
    fail "Ownership set incorrectly includes consumer-authored path: $consumer_file"
    OWNERSHIP_OK=false
  fi
done
if [[ "$OWNERSHIP_OK" == "true" ]]; then
  pass "Ownership set excludes consumer-authored paths"
fi

# Now exercise the actual install-loom.sh stale-file sweep end-to-end. We
# can't run the full installer in this temp repo (no gh / no loom-daemon
# binary path), but the sweep logic only depends on the inputs we
# already control (the metadata file and the new manifest). Reuse the
# find_stale_files helper from Section 5 — its case-statement carve-out
# matches install-loom.sh's, but it does NOT yet intersect against the
# ownership set. That's the bug Test 53 verifies fix in: apply the
# intersection manually here (mirrors the new install-loom.sh logic).
# A path NOT in the ownership set must NEVER appear in the stale list.
NEW_FILES_JSON='[".loom/scripts/worktree.sh",".loom/roles/builder.json"]'
RAW_STALE=$(find_stale_files "$OWNERSHIP_SWEEP_REPO/.loom/install-metadata.json" "$NEW_FILES_JSON")
FILTERED_STALE=""
while IFS= read -r candidate; do
  [[ -z "$candidate" ]] && continue
  if printf '%s\n' "$OWNERSHIP_SET" | grep -Fxq -- "$candidate"; then
    FILTERED_STALE="${FILTERED_STALE}${candidate}"$'\n'
  fi
done <<< "$RAW_STALE"

# Consumer paths must NOT be in the filtered stale list.
SWEEP_OK=true
for consumer_file in "$CONSUMER_SKILL" "$CONSUMER_COMMAND" "$CONSUMER_HOOK"; do
  if printf '%s' "$FILTERED_STALE" | grep -Fxq -- "$consumer_file"; then
    fail "Consumer path leaked into stale list after intersection: $consumer_file"
    SWEEP_OK=false
  fi
done
if [[ "$SWEEP_OK" == "true" ]]; then
  pass "Consumer paths excluded from stale list by ownership intersection"
fi

# The genuine Loom-shipped stale file (.loom/scripts/old-stale.sh) is NOT
# in the current ownership set either (it was removed from defaults/), so
# the intersection would also drop it. This is the documented trade-off —
# files Loom used to ship but no longer ships are preserved with a
# warning. Operators see the warning and can audit + manually clean up.
# This trade-off is acceptable because the alternative — trusting the
# legacy manifest unconditionally — is what caused the #3492 data loss.
if ! printf '%s' "$FILTERED_STALE" | grep -Fxq -- ".loom/scripts/old-stale.sh"; then
  pass "Genuinely stale Loom file also preserved (trade-off documented in #3492)"
else
  fail "Genuinely stale Loom file unexpectedly swept; intersection inverted?"
fi
echo ""

# Test 54: Uninstall preserves consumer files outside ownership set.
# End-to-end: stage a repo with an over-broad legacy manifest pointing at
# .claude/skills/anvil-memo/SKILL.md, .claude/commands/repo/lint.md, run
# uninstall-loom.sh --yes --local, assert the consumer files survive.
echo "Test 54: Uninstall preserves consumer files outside current ownership set"
OWNERSHIP_UNINSTALL_REPO="$TEST_DIR/ownership-uninstall-test"
create_temp_repo "$OWNERSHIP_UNINSTALL_REPO"
simulate_loom_install "$OWNERSHIP_UNINSTALL_REPO"

# Stage the same consumer-authored files.
mkdir -p "$OWNERSHIP_UNINSTALL_REPO/.claude/skills/anvil-memo"
mkdir -p "$OWNERSHIP_UNINSTALL_REPO/.claude/commands/repo"
echo "# Anvil memo skill (consumer)" > "$OWNERSHIP_UNINSTALL_REPO/$CONSUMER_SKILL"
echo "# Lint command (consumer)" > "$OWNERSHIP_UNINSTALL_REPO/$CONSUMER_COMMAND"
git -C "$OWNERSHIP_UNINSTALL_REPO" add -A
git -C "$OWNERSHIP_UNINSTALL_REPO" commit -m "consumer files" --quiet

# Inject an over-broad manifest that lists the consumer files alongside
# real Loom files. Mirrors the v0.7.x bug shape.
write_overbroad_manifest "$OWNERSHIP_UNINSTALL_REPO" \
  ".loom/config.json" \
  ".loom/roles/builder.json" \
  "$CONSUMER_SKILL" \
  "$CONSUMER_COMMAND"

git -C "$OWNERSHIP_UNINSTALL_REPO" add -A
git -C "$OWNERSHIP_UNINSTALL_REPO" commit -m "Over-broad manifest" --quiet 2>/dev/null || true

# Run uninstall and capture the output for the warning assertion.
UNINSTALL_OUTPUT=$("$UNINSTALL_SCRIPT" --yes --local "$OWNERSHIP_UNINSTALL_REPO" 2>&1 || true)

# Consumer files must survive.
if [[ -f "$OWNERSHIP_UNINSTALL_REPO/$CONSUMER_SKILL" ]]; then
  pass "Consumer .claude/skills/** path preserved across uninstall"
else
  fail "Consumer .claude/skills/anvil-memo/SKILL.md was deleted (#3492 regression)"
fi

if [[ -f "$OWNERSHIP_UNINSTALL_REPO/$CONSUMER_COMMAND" ]]; then
  pass "Consumer .claude/commands/repo/** path preserved across uninstall"
else
  fail "Consumer .claude/commands/repo/lint.md was deleted (#3492 regression)"
fi

# Genuine Loom-shipped paths in the manifest must still be removed.
if [[ ! -f "$OWNERSHIP_UNINSTALL_REPO/.loom/config.json" ]]; then
  pass "Loom-shipped .loom/config.json removed by uninstall (intersection allows Loom paths through)"
else
  fail ".loom/config.json still present after uninstall — intersection too aggressive?"
fi

# Warning text must surface to operators for each preserved path. The
# warning is the single-source-of-truth signal that the over-broad
# manifest is contaminated; silencing it would leave operators blind.
if echo "$UNINSTALL_OUTPUT" | grep -qF "preserving $CONSUMER_SKILL"; then
  pass "Warning emitted for preserved consumer skill path"
else
  fail "No 'preserving' warning emitted for $CONSUMER_SKILL"
fi
if echo "$UNINSTALL_OUTPUT" | grep -qF "preserving $CONSUMER_COMMAND"; then
  pass "Warning emitted for preserved consumer command path"
else
  fail "No 'preserving' warning emitted for $CONSUMER_COMMAND"
fi
echo ""


# ==========================================================================
# Section 11: version.sh discovery interface (#3468)
# ==========================================================================
# The /loom:release skill was retired in favor of /repo:release (#3563), but
# scripts/version.sh is retained — /repo:release detects and honors it as its
# first-priority version tool. These tests pin version.sh's list/check surface.

# Test 62: ./scripts/version.sh list emits the expected 5 entries
echo "Test 62: 'scripts/version.sh list' emits the 5 version-bearing files"
LIST_OUTPUT="$("$LOOM_ROOT/scripts/version.sh" list)"
EXPECTED_LIST="package.json
mcp-loom/package.json
loom-daemon/Cargo.toml
loom-api/Cargo.toml
CLAUDE.md"
if [[ "$LIST_OUTPUT" == "$EXPECTED_LIST" ]]; then
  pass "'version.sh list' emits the 5 expected files"
else
  fail "'version.sh list' output diverged from expectation"
  echo "  Expected:"
  echo "$EXPECTED_LIST" | sed 's/^/    /'
  echo "  Got:"
  echo "$LIST_OUTPUT" | sed 's/^/    /'
fi

# Test 63: ./scripts/version.sh check still works after the list addition
echo "Test 63: 'scripts/version.sh check' still works (regression)"
if "$LOOM_ROOT/scripts/version.sh" check >/dev/null 2>&1; then
  pass "'version.sh check' still works alongside the new 'list' subcommand"
else
  fail "'version.sh check' regressed after adding 'list'"
fi
echo ""


# ==========================================================================
# Section 12: Local-mode uninstall staging scope (#3545)
# ==========================================================================

# Test 64: Local-mode uninstall stages ONLY Loom-managed paths, never
# unrelated user changes. Regression guard for #3545: the old bare
# `git add -A` in Step 8 (local mode) swept in any pending user work —
# an in-progress edit or an embedded worktree — which the install.sh
# --quick reinstall path would then fold into its commit guidance.
echo "Test 64: Local uninstall stages only Loom paths, not user changes (#3545)"
SCOPE_REPO="$TEST_DIR/scoped-staging-test"
create_temp_repo "$SCOPE_REPO"
simulate_loom_install "$SCOPE_REPO"

# Commit a baseline that includes a tracked user file alongside the Loom install.
mkdir -p "$SCOPE_REPO/src"
echo "original" > "$SCOPE_REPO/src/app.txt"
git -C "$SCOPE_REPO" add -A
git -C "$SCOPE_REPO" commit -m "loom install + user file" --quiet

# Dirty the tree the way a user mid-edit would: modify a tracked file and drop
# an untracked file (mimics the .claude/worktrees/agent-*/ near-miss in #3545).
echo "user edit" >> "$SCOPE_REPO/src/app.txt"
mkdir -p "$SCOPE_REPO/user-junk"
echo "scratch" > "$SCOPE_REPO/user-junk/notes.txt"

"$UNINSTALL_SCRIPT" --yes --local "$SCOPE_REPO" > /dev/null 2>&1 || true

# The untracked user file must remain untracked/unstaged (?? in porcelain).
if git -C "$SCOPE_REPO" status --porcelain -- user-junk/notes.txt | grep -q '^??'; then
  pass "Untracked user file left unstaged by local uninstall (#3545)"
else
  fail "Untracked user file was staged by local uninstall (bare 'git add -A' regression, #3545)"
fi

# The modified tracked user file must remain a working-tree modification ( M).
if git -C "$SCOPE_REPO" status --porcelain -- src/app.txt | grep -q '^ M'; then
  pass "Modified tracked user file left unstaged by local uninstall (#3545)"
else
  fail "Modified tracked user file was staged by local uninstall (#3545)"
fi

# Loom file deletions MUST still be staged — that is the uninstall's job.
if git -C "$SCOPE_REPO" diff --staged --name-only | grep -q '^\.loom/'; then
  pass "Loom file deletions staged by local uninstall (scoped staging still works)"
else
  fail "Loom file deletions were not staged by local uninstall (#3545 over-scoped)"
fi
echo ""


# Test 65: Reinstall preserves consumer config.json keys (worktree.root) (#3598)
# A committed .loom/config.json carrying a `worktree.root` override must retain
# that key when the merge-aware daemon init runs over an existing consumer file
# (the reinstall path snapshots/restores config.json around the chained
# uninstall so init's merge sees it). This exercises the REAL `loom-daemon init`
# — the merge lives in loom-daemon::init::merge_config_file, which
# simulate_loom_install's bare `cp` does not cover. Also asserts idempotency:
# a second init leaves config.json byte-identical.
echo "Test 65: Reinstall preserves consumer config.json worktree.root override (#3598)"
DAEMON_BIN_65="$LOOM_ROOT/target/release/loom-daemon"
if [[ ! -x "$DAEMON_BIN_65" ]]; then
  warn "Skipping Test 65 — loom-daemon release binary not built at $DAEMON_BIN_65"
else
  CONFIG_MERGE_REPO="$TEST_DIR/config-merge-test"
  create_temp_repo "$CONFIG_MERGE_REPO"

  # Seed a committed consumer config.json with a load-bearing worktree.root
  # override plus an unknown consumer key, before Loom is installed.
  mkdir -p "$CONFIG_MERGE_REPO/.loom"
  cat > "$CONFIG_MERGE_REPO/.loom/config.json" <<'CFG_EOF'
{
  "version": "2",
  "worktree": { "root": "/Volumes/Stripe" },
  "customConsumerKey": "keep-me"
}
CFG_EOF

  if "$DAEMON_BIN_65" init --force --defaults "$LOOM_ROOT/defaults" "$CONFIG_MERGE_REPO" >/dev/null 2>&1; then
    MERGED_CFG="$CONFIG_MERGE_REPO/.loom/config.json"

    # The worktree.root override must survive the merge.
    if grep -q '/Volumes/Stripe' "$MERGED_CFG"; then
      pass "worktree.root override preserved through merge-aware init (#3598)"
    else
      fail "worktree.root override was dropped by init (#3598 regression)"
    fi

    # An unknown consumer key must survive too (deep merge, existing wins).
    if grep -q 'customConsumerKey' "$MERGED_CFG"; then
      pass "unknown consumer key preserved through merge-aware init (#3598)"
    else
      fail "unknown consumer key was dropped by init (#3598 regression)"
    fi

    # Newly shipped template keys must still be delivered on upgrade.
    if grep -q 'health_monitoring' "$MERGED_CFG"; then
      pass "template keys still delivered alongside preserved consumer keys (#3598)"
    else
      fail "template keys missing after merge (#3598)"
    fi

    # Idempotency: a second init must leave config.json byte-identical.
    CFG_AFTER_FIRST="$(cat "$MERGED_CFG")"
    "$DAEMON_BIN_65" init --force --defaults "$LOOM_ROOT/defaults" "$CONFIG_MERGE_REPO" >/dev/null 2>&1 || true
    CFG_AFTER_SECOND="$(cat "$MERGED_CFG")"
    if [[ "$CFG_AFTER_FIRST" == "$CFG_AFTER_SECOND" ]]; then
      pass "config.json merge is idempotent across repeat reinstalls (#3598)"
    else
      fail "config.json changed on a second reinstall (non-idempotent merge, #3598)"
    fi
  else
    fail "loom-daemon init failed against consumer repo with pre-existing config.json (#3598)"
  fi
fi
echo ""


# ==========================================================================
# Dogfood commands scoped-symlink (issue #3682)
# ==========================================================================
# The dogfood block in install-loom.sh only fires when TARGET == LOOM_ROOT
# (installing loom onto its own source repo), so the full installer cannot be
# exercised against a temp repo. The symlink logic is extracted into
# scripts/install/dogfood-commands.sh (`link_dogfood_commands`), which these
# tests source and drive directly in an isolated sandbox.
echo "=== Dogfood commands scoped-symlink (#3682) ==="

DOGFOOD_HELPER="$LOOM_ROOT/scripts/install/dogfood-commands.sh"

# Test 66: the helper exists and is sourceable.
echo "Test 66: dogfood-commands.sh helper exists and defines link_dogfood_commands"
if [[ -f "$DOGFOOD_HELPER" ]] && ( set +e; source "$DOGFOOD_HELPER"; declare -F link_dogfood_commands >/dev/null ); then
  pass "link_dogfood_commands is defined by scripts/install/dogfood-commands.sh"
else
  fail "link_dogfood_commands not found in scripts/install/dogfood-commands.sh"
fi

# Test 67: install-loom.sh no longer materializes a COPY, and calls the linker.
echo "Test 67: install-loom.sh uses the scoped symlink, not the old copy block"
if grep -q 'link_dogfood_commands "\$TARGET_PATH"' "$INSTALL_SCRIPT" \
   && ! grep -q 'Materialized .claude/commands/loom/ (real copy' "$INSTALL_SCRIPT"; then
  pass "install-loom.sh calls link_dogfood_commands and dropped the copy-and-swap"
else
  fail "install-loom.sh still materializes a copy or does not call link_dogfood_commands"
fi

# Build an isolated sandbox that mimics a loom source repo: a defaults/ tree
# plus a real .claude/commands/ destination dir.
DOGFOOD_SANDBOX="$TEST_DIR/dogfood-sandbox"
mkdir -p "$DOGFOOD_SANDBOX/defaults/.claude/commands/loom"
echo "builder source of truth" > "$DOGFOOD_SANDBOX/defaults/.claude/commands/loom/builder.md"
echo "judge source of truth" > "$DOGFOOD_SANDBOX/defaults/.claude/commands/loom/judge.md"

# Drive the helper in a subshell so its fallback logging funcs don't leak.
(
  set +e
  source "$DOGFOOD_HELPER"
  link_dogfood_commands "$DOGFOOD_SANDBOX"
) > /dev/null 2>&1

CMD_LOOM_LINK="$DOGFOOD_SANDBOX/.claude/commands/loom"

# Test 68: `.claude/commands/loom` is a symlink to the relative defaults path.
echo "Test 68: .claude/commands/loom is a relative symlink into defaults/"
if [[ -L "$CMD_LOOM_LINK" ]] && [[ "$(readlink "$CMD_LOOM_LINK")" == "../../defaults/.claude/commands/loom" ]]; then
  pass ".claude/commands/loom -> ../../defaults/.claude/commands/loom"
else
  fail ".claude/commands/loom is not the expected relative symlink (got: $(readlink "$CMD_LOOM_LINK" 2>/dev/null || echo '<not a symlink>'))"
fi

# Test 69: `.claude/commands/` itself stays a REAL directory (not a symlink).
echo "Test 69: .claude/commands parent stays a real directory"
if [[ -d "$DOGFOOD_SANDBOX/.claude/commands" ]] && [[ ! -L "$DOGFOOD_SANDBOX/.claude/commands" ]]; then
  pass ".claude/commands is a real directory (parent not symlinked)"
else
  fail ".claude/commands is missing or is itself a symlink"
fi

# Test 70: content resolves through the symlink to defaults/ (no drift possible).
echo "Test 70: command content resolves through the symlink to defaults/"
if [[ "$(cat "$CMD_LOOM_LINK/builder.md" 2>/dev/null)" == "builder source of truth" ]]; then
  pass "reads through the symlink return the defaults/ source of truth"
else
  fail "content behind .claude/commands/loom/builder.md did not resolve to defaults/"
fi

# Test 71: #3565 safety — a co-installed tool writing a SIBLING namespace does
# NOT pollute defaults/, and does NOT write through the loom symlink.
echo "Test 71: sibling namespace write does not pollute defaults/ (#3565 safety)"
mkdir -p "$DOGFOOD_SANDBOX/.claude/commands/repo"
echo "repo lint command" > "$DOGFOOD_SANDBOX/.claude/commands/repo/lint.md"
if [[ -f "$DOGFOOD_SANDBOX/.claude/commands/repo/lint.md" ]] \
   && [[ ! -e "$DOGFOOD_SANDBOX/defaults/.claude/commands/repo" ]]; then
  pass "sibling .claude/commands/repo/ is a real dir; defaults/ untouched"
else
  fail "sibling namespace leaked into defaults/ (#3565 regression)"
fi

# Test 72: idempotent — re-running leaves the symlink correct and unchanged.
echo "Test 72: link_dogfood_commands is idempotent"
(
  set +e
  source "$DOGFOOD_HELPER"
  link_dogfood_commands "$DOGFOOD_SANDBOX"
) > /dev/null 2>&1
if [[ -L "$CMD_LOOM_LINK" ]] && [[ "$(readlink "$CMD_LOOM_LINK")" == "../../defaults/.claude/commands/loom" ]]; then
  pass "second invocation keeps the symlink correct"
else
  fail "second invocation left the symlink in an unexpected state"
fi

# Test 73: replaces a pre-existing real (stale copy) directory with the symlink.
echo "Test 73: a stale real copy is replaced by the symlink"
DOGFOOD_SANDBOX2="$TEST_DIR/dogfood-sandbox2"
mkdir -p "$DOGFOOD_SANDBOX2/defaults/.claude/commands/loom"
echo "fresh builder" > "$DOGFOOD_SANDBOX2/defaults/.claude/commands/loom/builder.md"
# Pre-seed a stale materialized copy (byte-different from defaults).
mkdir -p "$DOGFOOD_SANDBOX2/.claude/commands/loom"
echo "STALE builder copy" > "$DOGFOOD_SANDBOX2/.claude/commands/loom/builder.md"
(
  set +e
  source "$DOGFOOD_HELPER"
  link_dogfood_commands "$DOGFOOD_SANDBOX2"
) > /dev/null 2>&1
CMD_LOOM_LINK2="$DOGFOOD_SANDBOX2/.claude/commands/loom"
if [[ -L "$CMD_LOOM_LINK2" ]] && [[ "$(cat "$CMD_LOOM_LINK2/builder.md")" == "fresh builder" ]]; then
  pass "stale real copy replaced by symlink resolving to defaults/"
else
  fail "stale real copy was not replaced by the symlink"
fi

# Test 74: local-only files in the stale copy are preserved (not silently lost).
echo "Test 74: refuses to clobber local-only files not present in defaults/"
DOGFOOD_SANDBOX3="$TEST_DIR/dogfood-sandbox3"
mkdir -p "$DOGFOOD_SANDBOX3/defaults/.claude/commands/loom"
echo "builder" > "$DOGFOOD_SANDBOX3/defaults/.claude/commands/loom/builder.md"
mkdir -p "$DOGFOOD_SANDBOX3/.claude/commands/loom"
echo "builder" > "$DOGFOOD_SANDBOX3/.claude/commands/loom/builder.md"
echo "local only" > "$DOGFOOD_SANDBOX3/.claude/commands/loom/local-only.md"
(
  set +e
  source "$DOGFOOD_HELPER"
  link_dogfood_commands "$DOGFOOD_SANDBOX3"
) > /dev/null 2>&1
CMD_LOOM_LINK3="$DOGFOOD_SANDBOX3/.claude/commands/loom"
if [[ ! -L "$CMD_LOOM_LINK3" ]] && [[ -f "$CMD_LOOM_LINK3/local-only.md" ]]; then
  pass "local-only file preserved; refused to replace with symlink"
else
  fail "local-only file lost or dir replaced despite local-only content"
fi

# Test 75: a legacy whole-dir .claude/commands symlink is removed and rebuilt.
echo "Test 75: legacy whole-dir .claude/commands symlink is replaced"
DOGFOOD_SANDBOX4="$TEST_DIR/dogfood-sandbox4"
mkdir -p "$DOGFOOD_SANDBOX4/defaults/.claude/commands/loom"
echo "builder" > "$DOGFOOD_SANDBOX4/defaults/.claude/commands/loom/builder.md"
mkdir -p "$DOGFOOD_SANDBOX4/.claude"
# Legacy: whole .claude/commands is a symlink into defaults/.claude/commands.
mkdir -p "$DOGFOOD_SANDBOX4/defaults/.claude/commands"
( cd "$DOGFOOD_SANDBOX4/.claude" && ln -s "../defaults/.claude/commands" commands )
(
  set +e
  source "$DOGFOOD_HELPER"
  link_dogfood_commands "$DOGFOOD_SANDBOX4"
) > /dev/null 2>&1
if [[ ! -L "$DOGFOOD_SANDBOX4/.claude/commands" ]] \
   && [[ -d "$DOGFOOD_SANDBOX4/.claude/commands" ]] \
   && [[ -L "$DOGFOOD_SANDBOX4/.claude/commands/loom" ]]; then
  pass "legacy whole-dir symlink removed; parent real, loom/ symlinked"
else
  fail "legacy whole-dir .claude/commands symlink was not correctly replaced"
fi

echo ""


# ==========================================================================
# Test: check-phantom-labels.sh (role prompts reference only real labels, #3786)
# ==========================================================================
echo "Test: check-phantom-labels.sh detects phantom labels and passes the real tree"
PHANTOM_LINT="$DEFAULTS_DIR/scripts/check-phantom-labels.sh"
if [[ ! -x "$PHANTOM_LINT" ]]; then
  fail "check-phantom-labels.sh missing or not executable"
else
  # (a) The real defaults/ tree must be clean.
  if bash "$PHANTOM_LINT" "$LOOM_ROOT" >/dev/null 2>&1; then
    pass "check-phantom-labels passes against the real defaults/ tree"
  else
    fail "check-phantom-labels flagged the real defaults/ tree (should be clean)"
  fi

  # (b) A fixture with an injected phantom label in application context must fail.
  PHANTOM_FIX="$(mktemp -d)"
  mkdir -p "$PHANTOM_FIX/.github" "$PHANTOM_FIX/defaults/.github" "$PHANTOM_FIX/defaults/roles"
  printf -- '- name: loom:issue\n  color: "3B82F6"\n' > "$PHANTOM_FIX/.github/labels.yml"
  printf -- '- name: loom:issue\n  color: "3B82F6"\n' > "$PHANTOM_FIX/defaults/.github/labels.yml"
  printf 'Do this: gh issue edit 1 --add-label "loom:ghost-label"\n' > "$PHANTOM_FIX/defaults/roles/x.md"
  PHANTOM_OUT="$(bash "$PHANTOM_LINT" "$PHANTOM_FIX" 2>&1)" && PHANTOM_RC=0 || PHANTOM_RC=$?
  if [[ "$PHANTOM_RC" -ne 0 ]] && echo "$PHANTOM_OUT" | grep -q "loom:ghost-label"; then
    pass "check-phantom-labels fails (exit $PHANTOM_RC) and names the injected phantom label"
  else
    fail "check-phantom-labels did not catch the injected phantom label (rc=$PHANTOM_RC)"
  fi

  # (c) The same fixture with a real label in application context must pass —
  #     the /loom:sweep command name and a prose-only label mention (each on a
  #     line WITHOUT a label-application flag) are structurally ignored, so
  #     neither false-positives even though they are not in the fixture registry.
  {
    printf 'Run /loom:sweep for the full lifecycle.\n'
    printf 'Mind the `loom:curating` label, which prevents Curator overlap.\n'
    printf 'Then apply the real label: gh issue edit 1 --add-label "loom:issue"\n'
  } > "$PHANTOM_FIX/defaults/roles/x.md"
  if bash "$PHANTOM_LINT" "$PHANTOM_FIX" >/dev/null 2>&1; then
    pass "check-phantom-labels passes on a real label and ignores /loom:sweep + prose"
  else
    fail "check-phantom-labels false-positived on a real label or command name"
  fi
  rm -rf "$PHANTOM_FIX"
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
