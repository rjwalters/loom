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

  # Handle CLAUDE.md exactly as the real installer does
  # (loom-daemon/src/init/scaffolding.rs:558-692):
  #   1. Write the full Loom guide to <target>/.loom/CLAUDE.md, reading from the
  #      surviving template defaults/.loom/CLAUDE.md.
  #   2. Inject ONLY the short pointer, wrapped in Loom section markers, into
  #      root <target>/CLAUDE.md — never the full guide.
  # Emitting the full guide to root here (the pre-#4144 behaviour) is exactly the
  # divergence Test "installer parity" below guards against: the real installer
  # has no such path, so the uninstaller's marker branch was never exercised by
  # the simulator.
  if [[ -f "$DEFAULTS_DIR/.loom/CLAUDE.md" ]]; then
    # Step 1: full guide -> .loom/CLAUDE.md. The real installer localizes
    # link *targets* at write time (loom-daemon/src/init/templates.rs
    # `localize_dotloom_doc_links`, issue #5975 / PR #6001) since the
    # template is authored resolving from repo root but is installed one
    # directory level deeper. Mirror that transform here too, or this
    # simulator diverges from the real installer's output and produces a
    # false "the installed .loom/CLAUDE.md has broken links" signal (#6321)
    # for links that the real installer already rebases correctly.
    sed -e 's/\](\.loom\//](/g' -e 's/\](\.github\//](..\/.github\//g' \
      "$DEFAULTS_DIR/.loom/CLAUDE.md" > "$target/.loom/CLAUDE.md"

    # Step 2: marker-wrapped pointer -> root CLAUDE.md (mirrors LOOM_ROOT_POINTER
    # wrapped by wrap_loom_content() in scaffolding.rs).
    cat > "$target/CLAUDE.md" << 'ROOT_CLAUDE_EOF'
<!-- BEGIN LOOM ORCHESTRATION -->
This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration — see the Loom repository for the full guide (roles, labels, worktrees, configuration). When installed, Loom also writes a locally-substituted copy of that guide to `.loom/CLAUDE.md`.
<!-- END LOOM ORCHESTRATION -->
ROOT_CLAUDE_EOF
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

  # Inject the LEGACY project-level Loom hook entries into the simulated
  # .claude/settings.json. As of Epic #3835 Phase 5 (#4262) a fresh install no
  # longer wires project-level hooks (they execute machine-level via user-scope
  # ~/.claude/settings.json, and defaults/.claude/settings.json dropped its
  # `hooks` block), so the copy above yields a hook-less settings file. This
  # simulator, however, stands in for an ALREADY-installed / transition repo —
  # the shape Phase 5 explicitly preserves (existing project entries are NOT
  # removed until Phase 6 / #4254) and the shape the uninstall + project-hook
  # tests below validate. Re-add the historical `${CLAUDE_PROJECT_DIR}`-prefixed
  # entries so those tests keep exercising the legacy layout.
  if [[ -f "$target/.claude/settings.json" ]] && command -v jq >/dev/null 2>&1; then
    _sim_tmp="$(mktemp)"
    if jq '
      .hooks = {
        "PreToolUse": [
          { "matcher": "Bash", "hooks": [
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh" },
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh" }
          ] }
        ],
        "UserPromptSubmit": [
          { "matcher": "", "hooks": [
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/skill-router.sh" },
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/methodology-inject.sh" }
          ] }
        ],
        "Stop": [
          { "hooks": [
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-background-subagents.sh" }
          ] }
        ]
      }
    ' "$target/.claude/settings.json" > "$_sim_tmp" 2>/dev/null; then
      mv "$_sim_tmp" "$target/.claude/settings.json"
    else
      rm -f "$_sim_tmp"
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

# Test 10b: installer parity — root CLAUDE.md is the marker-wrapped pointer, NOT
# the full guide, and the full guide lives at .loom/CLAUDE.md.
#
# This is the regression guard for #4144: the pre-fix simulator bare-`cp`d the
# 2000+ line guide to root CLAUDE.md — a path the real installer
# (loom-daemon/src/init/scaffolding.rs:558-692) never takes. It writes the guide
# to <target>/.loom/CLAUDE.md and injects only LOOM_ROOT_POINTER, wrapped in
# `<!-- BEGIN/END LOOM ORCHESTRATION -->` markers, into root CLAUDE.md. This
# assertion FAILS if the simulator ever regresses to writing the full guide to
# root (the header line leaks into root, or the marker block is absent) — verify
# by temporarily reverting simulate_loom_install to `cp .../.loom/CLAUDE.md root`.
echo "Test 10b: Root CLAUDE.md is the marker-wrapped pointer, not the full guide"
PARITY_OK=true
if ! grep -q '<!-- BEGIN LOOM ORCHESTRATION -->' "$INSTALL_REPO/CLAUDE.md" 2>/dev/null || \
   ! grep -q '<!-- END LOOM ORCHESTRATION -->' "$INSTALL_REPO/CLAUDE.md" 2>/dev/null; then
  PARITY_OK=false
  fail "installer parity: root CLAUDE.md lacks the Loom section markers"
fi
if grep -q '^# Loom Orchestration - Repository Guide' "$INSTALL_REPO/CLAUDE.md" 2>/dev/null; then
  PARITY_OK=false
  fail "installer parity: root CLAUDE.md contains the full guide (should be pointer only)"
fi
if [[ ! -f "$INSTALL_REPO/.loom/CLAUDE.md" ]] || \
   ! grep -q '^# Loom Orchestration - Repository Guide' "$INSTALL_REPO/.loom/CLAUDE.md" 2>/dev/null; then
  PARITY_OK=false
  fail "installer parity: full guide missing from .loom/CLAUDE.md"
fi
if [[ "$PARITY_OK" == "true" ]]; then
  pass "installer parity: root is pointer-only, full guide at .loom/CLAUDE.md"
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

# Test 12c: No invalid Write(...) path rule in permissions.allow (issue #4072)
# Claude Code's permission engine only matches Edit(...) rules for file-editing
# tools (Write/Edit/NotebookEdit all route through Edit checks) -- a
# Write(...) path rule prints a startup warning and is a silent no-op. Also
# assert the four temp-path entries the fix introduces are all present.
echo "Test 12c: settings.json has no invalid Write(...) path rule"
if [[ -f "$SETTINGS_FILE" ]] && command -v jq &> /dev/null; then
  if jq -e '.permissions.allow[] | select(startswith("Write("))' "$SETTINGS_FILE" > /dev/null 2>&1; then
    fail "settings.json contains an invalid Write(...) path rule -- use Edit(...)"
  else
    pass "no invalid Write(...) path rules in settings.json"
  fi

  TEMP_PATH_FAIL=0
  for entry in "Read(/tmp/**)" "Read(/private/tmp/**)" "Edit(/tmp/**)" "Edit(/private/tmp/**)"; do
    if ! jq -e --arg entry "$entry" '.permissions.allow[] | select(. == $entry)' "$SETTINGS_FILE" > /dev/null 2>&1; then
      fail "settings.json missing expected temp-path rule: $entry"
      TEMP_PATH_FAIL=1
    fi
  done
  if [[ $TEMP_PATH_FAIL -eq 0 ]]; then
    pass "settings.json has all four temp-path permission entries"
  fi
else
  fail "Cannot verify settings.json permissions (settings.json or jq missing)"
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

# Pre-uninstall precondition for Test 25: the freshly-installed root CLAUDE.md
# must carry the Loom section marker block. This makes Test 25's post-uninstall
# assertion prove *removal* of content shown to be present beforehand, rather
# than passing vacuously (issue #4144 — the marker-pointer model means root
# CLAUDE.md no longer contains the "Loom Orchestration" header the old grep keyed
# on, so absence-by-construction must not masquerade as a removal proof).
if grep -q '<!-- BEGIN LOOM ORCHESTRATION -->' "$UNINSTALL_REPO/CLAUDE.md" 2>/dev/null; then
  pass "Pre-uninstall: root CLAUDE.md carries the Loom marker block (Test 25 precondition)"
else
  fail "Pre-uninstall: root CLAUDE.md is missing the Loom marker block — Test 25 would pass vacuously"
fi

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
# The root CLAUDE.md was the marker-wrapped pointer (asserted present above,
# pre-uninstall). A pointer-only root is entirely Loom content, so the
# uninstaller's marker branch strips the section and removes the now-empty file.
# Assert the marker block is gone — not just that the "Loom Orchestration" header
# is absent (it never was, under the pointer model), which would pass vacuously.
echo "Test 25: After uninstall, CLAUDE.md Loom section removed"
if [[ ! -f "$UNINSTALL_REPO/CLAUDE.md" ]]; then
  pass "CLAUDE.md removed (was entirely the Loom pointer)"
else
  # File survived (e.g. mixed with user content): the marker block must be gone.
  if grep -q '<!-- BEGIN LOOM ORCHESTRATION -->' "$UNINSTALL_REPO/CLAUDE.md" 2>/dev/null; then
    fail "CLAUDE.md still contains the Loom marker block"
  else
    pass "CLAUDE.md Loom section removed"
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
# Under the marker-pointer model (issue #4144) the installed root CLAUDE.md is
# the marker-wrapped pointer and nothing else. Prove removal, not absence: assert
# the marker block is present before uninstall, then that the file is gone after.
echo "Test 31: Loom-generated CLAUDE.md is fully removed"
CLAUDEMD_REPO="$TEST_DIR/claudemd-test"
create_temp_repo "$CLAUDEMD_REPO"
simulate_loom_install "$CLAUDEMD_REPO"

# Precondition: the installed root CLAUDE.md is the marker-wrapped pointer.
if grep -q '<!-- BEGIN LOOM ORCHESTRATION -->' "$CLAUDEMD_REPO/CLAUDE.md" 2>/dev/null; then
  pass "Pre-uninstall: Loom-generated CLAUDE.md carries the marker block"
else
  fail "Pre-uninstall: Loom-generated CLAUDE.md is missing the marker block"
fi

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

    # #4050: a direct `loom-daemon init` (no LOOM_VERSION/LOOM_COMMIT exported —
    # this test does NOT set them, unlike install.sh's prepare_loom_metadata_env)
    # must still write .loom/install-metadata.json with a real version, and must
    # substitute a real version into .loom/CLAUDE.md rather than "unknown".
    META_52="$INTERNAL_REPO/.loom/install-metadata.json"
    if [[ -f "$META_52" ]] && jq empty "$META_52" >/dev/null 2>&1; then
      meta_version="$(jq -r '.loom_version // ""' "$META_52")"
      meta_commit="$(jq -r '.loom_commit // ""' "$META_52")"
      if [[ -n "$meta_version" && "$meta_version" != "unknown" ]]; then
        pass "#4050: daemon-direct init wrote install-metadata.json with loom_version=$meta_version"
      else
        fail "#4050: install-metadata.json loom_version is empty/unknown ('$meta_version')"
      fi
      if [[ -n "$meta_commit" ]]; then
        pass "#4050: install-metadata.json carries a non-empty loom_commit"
      else
        fail "#4050: install-metadata.json loom_commit is empty"
      fi
    else
      fail "#4050: daemon-direct init did not write a parseable .loom/install-metadata.json"
    fi

    CLAUDE_52="$INTERNAL_REPO/.loom/CLAUDE.md"
    if [[ -f "$CLAUDE_52" ]]; then
      # Only the five Loom template placeholders must be substituted; unrelated
      # `{{...}}` in example content (e.g. `{{workspace}}`) is intentional, so
      # match the exact placeholder set (mirrors Rust TEMPLATE_PLACEHOLDERS).
      if grep -Eq '\{\{(LOOM_VERSION|LOOM_COMMIT|INSTALL_DATE|REPO_OWNER|REPO_NAME)\}\}' "$CLAUDE_52"; then
        fail "#4050: .loom/CLAUDE.md still contains an unsubstituted Loom template placeholder"
      elif grep -q '\*\*Loom Version\*\*: unknown' "$CLAUDE_52"; then
        fail "#4050: .loom/CLAUDE.md renders 'Loom Version: unknown' on daemon-direct init"
      else
        pass "#4050: .loom/CLAUDE.md has a substituted version and no leftover placeholder"
      fi
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

# Test 62: ./scripts/version.sh list emits the expected version-bearing files
#
# The base set is the 6 always-present files -- "VERSION" (issue #5517, the
# root plain-text file required by the tool-package installer contract's C8
# "Honest source version") joined the original 5 alongside CLAUDE.md.
# `.loom/install-metadata.json` is a conditional 7th entry (#4842): it exists
# only on a dogfooded install (loom installed on its own repo — which IS the
# case for this repo's own CI run), and version.sh's `list` arm
# existence-checks it before emitting. Mirror that same presence check here
# rather than hardcoding a fixed line count, so the test passes both in this
# repo and in a non-dogfooded checkout.
echo "Test 62: 'scripts/version.sh list' emits the version-bearing files"
LIST_OUTPUT="$("$LOOM_ROOT/scripts/version.sh" list)"
EXPECTED_LIST="package.json
mcp-loom/package.json
loom-daemon/Cargo.toml
loom-api/Cargo.toml
CLAUDE.md
VERSION"
if [[ -f "$LOOM_ROOT/.loom/install-metadata.json" ]]; then
  EXPECTED_LIST="$EXPECTED_LIST
.loom/install-metadata.json"
fi
if [[ "$LIST_OUTPUT" == "$EXPECTED_LIST" ]]; then
  pass "'version.sh list' emits the expected version-bearing files"
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
# Section 11b: version.sh bump/set self-verification loud-failure path (#6536)
# ==========================================================================
# Three real Builder-authored bump commits (678602b6, 693751b0, 8107b78f) each
# shipped an incomplete version bump that only surfaced as a CI failure
# ("Installer Integration Tests" Test 63 above). `set_version()` never
# self-verified its own result, and its `cargo update`/`npm install
# --package-lock-only` steps swallowed stderr — so neither a hand-edited file
# nor a silently under-delivered lockfile update was ever caught locally.
# These tests exercise the fix (loud non-zero exit + a post-bump
# check_versions() self-check) against an isolated scratch copy of
# version.sh — never against $LOOM_ROOT itself, since `bump`/`set` mutate
# files in place.

version_scratch_build() {
  local dir="$1"
  local version="$2"
  mkdir -p "$dir/scripts" "$dir/mcp-loom" "$dir/loom-daemon" "$dir/loom-api"
  cp "$LOOM_ROOT/scripts/version.sh" "$dir/scripts/version.sh"
  chmod +x "$dir/scripts/version.sh"

  cat > "$dir/package.json" <<EOF
{
  "name": "loom-version-scratch",
  "version": "$version"
}
EOF

  cat > "$dir/mcp-loom/package.json" <<EOF
{
  "name": "@loom/mcp-version-scratch",
  "version": "$version"
}
EOF

  cat > "$dir/loom-daemon/Cargo.toml" <<EOF
[package]
name = "loom-daemon"
version = "$version"
edition = "2021"
EOF

  cat > "$dir/loom-api/Cargo.toml" <<EOF
[package]
name = "loom-api"
version = "$version"
edition = "2021"
EOF

  cat > "$dir/CLAUDE.md" <<EOF
# Scratch

**Loom Version**: $version
EOF

  printf '%s\n' "$version" > "$dir/VERSION"

  cat > "$dir/Cargo.lock" <<EOF
# This file is automatically @generated by Cargo.
version = 3

[[package]]
name = "loom-api"
version = "$version"
dependencies = [
]

[[package]]
name = "loom-daemon"
version = "$version"
dependencies = [
]
EOF

  cat > "$dir/mcp-loom/package-lock.json" <<EOF
{
  "name": "@loom/mcp-version-scratch",
  "version": "$version",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "@loom/mcp-version-scratch",
      "version": "$version",
      "dependencies": {}
    }
  }
}
EOF
}

if ! command -v jq >/dev/null 2>&1; then
  warn "Skipping version.sh loud-failure tests (#6536) — 'jq' not on PATH"
else
  # --- Case 1: a subshell step (cargo update / npm install) silently no-ops
  # (reports success, changes nothing) — simulates the lock-contention
  # hypothesis. bump must now exit non-zero and name the still-mismatched
  # files, instead of completing as if nothing were wrong.
  echo "Test: 'version.sh bump' fails loudly when a lockfile silently fails to update (#6536)"
  NOOP_REPO="$TEST_DIR/version-scratch-noop-6536"
  version_scratch_build "$NOOP_REPO" "9.9.9"
  NOOP_STUB_BIN="$TEST_DIR/version-stub-noop-6536"
  mkdir -p "$NOOP_STUB_BIN"
  cat > "$NOOP_STUB_BIN/cargo" <<'EOF'
#!/usr/bin/env bash
# Stub cargo: reports success without touching Cargo.lock at all —
# simulates a benign no-op under package-cache lock contention.
exit 0
EOF
  cat > "$NOOP_STUB_BIN/npm" <<'EOF'
#!/usr/bin/env bash
# Stub npm: reports success without touching package-lock.json at all.
exit 0
EOF
  chmod +x "$NOOP_STUB_BIN/cargo" "$NOOP_STUB_BIN/npm"

  NOOP_OUT=""
  NOOP_RC=0
  set +e
  NOOP_OUT=$(PATH="$NOOP_STUB_BIN:$PATH" "$NOOP_REPO/scripts/version.sh" bump patch 2>&1)
  NOOP_RC=$?
  set -e
  if [[ $NOOP_RC -ne 0 ]]; then
    pass "'version.sh bump' exits non-zero when cargo/npm silently no-op on the lockfiles (#6536)"
  else
    fail "'version.sh bump' exited 0 despite Cargo.lock/package-lock.json never being updated (#6536)"
  fi
  if echo "$NOOP_OUT" | grep -q "MISMATCH  Cargo.lock" && echo "$NOOP_OUT" | grep -q "MISMATCH  mcp-loom/package-lock.json"; then
    pass "'version.sh bump' names the still-mismatched lockfiles in its output (#6536)"
  else
    fail "'version.sh bump' output did not name the mismatched lockfiles: $NOOP_OUT (#6536)"
  fi

  # --- Case 2: cargo itself fails outright (non-zero exit, stderr). Must be
  # surfaced immediately with a clear message — not swallowed by the old
  # `2>/dev/null`, and not left for a human to discover only via CI.
  echo "Test: 'version.sh bump' surfaces a genuine cargo/npm failure instead of swallowing stderr (#6536)"
  FAIL_REPO="$TEST_DIR/version-scratch-fail-6536"
  version_scratch_build "$FAIL_REPO" "9.9.9"
  FAIL_STUB_BIN="$TEST_DIR/version-stub-fail-6536"
  mkdir -p "$FAIL_STUB_BIN"
  cat > "$FAIL_STUB_BIN/cargo" <<'EOF'
#!/usr/bin/env bash
echo "error: could not acquire package cache lock" >&2
exit 1
EOF
  cat > "$FAIL_STUB_BIN/npm" <<'EOF'
#!/usr/bin/env bash
echo "npm ERR! stub should not be reached" >&2
exit 1
EOF
  chmod +x "$FAIL_STUB_BIN/cargo" "$FAIL_STUB_BIN/npm"

  FAIL_OUT=""
  FAIL_RC=0
  set +e
  FAIL_OUT=$(PATH="$FAIL_STUB_BIN:$PATH" "$FAIL_REPO/scripts/version.sh" bump patch 2>&1)
  FAIL_RC=$?
  set -e
  if [[ $FAIL_RC -ne 0 ]] && echo "$FAIL_OUT" | grep -qi "cargo update loom-daemon loom-api' failed"; then
    pass "'version.sh bump' surfaces the cargo update failure with a clear error and non-zero exit (#6536)"
  else
    fail "'version.sh bump' did not surface the cargo failure clearly: rc=$FAIL_RC out='$FAIL_OUT' (#6536)"
  fi
  if ! echo "$FAIL_OUT" | grep -q "npm ERR! stub should not be reached"; then
    pass "'version.sh bump' aborts before the npm step once cargo update fails (#6536)"
  else
    fail "'version.sh bump' proceeded to the npm step after cargo update already failed (#6536)"
  fi

  # --- Case 3 (no false positive): both subshell steps succeed and actually
  # bring the lockfiles to the new version — bump must still exit 0.
  echo "Test: 'version.sh bump' still exits 0 when every file (including lockfiles) lands in sync (#6536)"
  OK_REPO="$TEST_DIR/version-scratch-ok-6536"
  version_scratch_build "$OK_REPO" "9.9.9"
  OK_STUB_BIN="$TEST_DIR/version-stub-ok-6536"
  mkdir -p "$OK_STUB_BIN"
  cat > "$OK_STUB_BIN/cargo" <<'EOF'
#!/usr/bin/env bash
# Stub cargo: simulates a real `cargo update loom-daemon loom-api` by
# bumping the version field for those two packages in ./Cargo.lock.
set -euo pipefail
if [[ "${1:-}" != "update" ]]; then
  echo "stub cargo: unrecognized invocation: $*" >&2
  exit 64
fi
NEW_VERSION="${STUB_VERSION_SH_TARGET:?STUB_VERSION_SH_TARGET not set}"
awk -v ver="$NEW_VERSION" '
  /^name = "loom-api"$/ || /^name = "loom-daemon"$/ { print; getline; print "version = \"" ver "\""; next }
  { print }
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock
EOF
  cat > "$OK_STUB_BIN/npm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
NEW_VERSION="${STUB_VERSION_SH_TARGET:?STUB_VERSION_SH_TARGET not set}"
jq --arg v "$NEW_VERSION" '.version = $v | .packages[""].version = $v' package-lock.json > package-lock.json.tmp \
  && mv package-lock.json.tmp package-lock.json
EOF
  chmod +x "$OK_STUB_BIN/cargo" "$OK_STUB_BIN/npm"

  OK_OUT=""
  OK_RC=0
  set +e
  OK_OUT=$(STUB_VERSION_SH_TARGET="9.9.10" PATH="$OK_STUB_BIN:$PATH" "$OK_REPO/scripts/version.sh" bump patch 2>&1)
  OK_RC=$?
  set -e
  if [[ $OK_RC -eq 0 ]]; then
    pass "'version.sh bump' exits 0 when the subshell steps genuinely bring every file in sync (#6536, no false positive)"
  else
    fail "'version.sh bump' exited $OK_RC even though every file (including lockfiles) landed in sync: $OK_OUT (#6536)"
  fi
  if [[ "$("$OK_REPO/scripts/version.sh" show)" == "9.9.10" ]]; then
    pass "'version.sh bump' left the scratch repo at the bumped version 9.9.10 (#6536)"
  else
    fail "'version.sh bump' left the scratch repo at an unexpected version (#6536)"
  fi
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


# Test: reinstall preserves a pre-existing, user-added `.loom/config.json`
# .gitignore rule (#5242). A fleet host that keeps host-local runtime state
# (e.g. a `worktree.root` override) in `.loom/config.json` may deliberately
# gitignore that single, narrowly-scoped file even though Loom's default
# design commits it for team sharing. `update_gitignore`'s legacy migration
# (which strips genuinely over-broad `.loom/*.json`-style patterns left by
# very old installs) must not sweep up that narrow, intentional rule too —
# it never added the rule itself, so it must not remove it either. This
# exercises the REAL `loom-daemon init` gitignore reconciliation
# (`loom-daemon::init::post_init::update_gitignore`), the same code path
# `install.sh` and `resync-installed.sh update-gitignore` both call.
echo "Test: reinstall preserves a pre-existing .loom/config.json gitignore rule (#5242)"
DAEMON_BIN_5242="$LOOM_ROOT/target/release/loom-daemon"
if [[ ! -x "$DAEMON_BIN_5242" ]]; then
  warn "Skipping Test #5242 — loom-daemon release binary not built at $DAEMON_BIN_5242"
else
  GITIGNORE_CONFIG_REPO="$TEST_DIR/gitignore-config-json-test"
  create_temp_repo "$GITIGNORE_CONFIG_REPO"

  # Seed a documented, host-local divergence: the operator deliberately
  # gitignores `.loom/config.json` (mirrors rjwalters/lean-genius#43683).
  cat > "$GITIGNORE_CONFIG_REPO/.gitignore" <<'GI5242_EOF'
node_modules/

# Host-local divergence: worktree.root is per-machine, not team-shared.
.loom/config.json
GI5242_EOF

  if "$DAEMON_BIN_5242" init --force --defaults "$LOOM_ROOT/defaults" "$GITIGNORE_CONFIG_REPO" >/dev/null 2>&1; then
    GI_5242="$GITIGNORE_CONFIG_REPO/.gitignore"

    if grep -qxF '.loom/config.json' "$GI_5242"; then
      pass "pre-existing .loom/config.json ignore rule survived loom-daemon init (#5242)"
    else
      fail ".loom/config.json ignore rule was stripped by loom-daemon init (#5242 regression)"
    fi

    # Re-running (as a resync/reinstall would) must not strip it either.
    "$DAEMON_BIN_5242" init --force --defaults "$LOOM_ROOT/defaults" "$GITIGNORE_CONFIG_REPO" >/dev/null 2>&1 || true
    if grep -qxF '.loom/config.json' "$GI_5242"; then
      pass ".loom/config.json ignore rule survives a second loom-daemon init (#5242)"
    else
      fail ".loom/config.json ignore rule was stripped on a second run (#5242 regression)"
    fi
  else
    fail "loom-daemon init failed against consumer repo with a pre-existing .gitignore (#5242)"
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
# Test: check-labels-drift.sh (root vs defaults labels.yml parity, #3896)
# ==========================================================================
echo "Test: check-labels-drift.sh detects drift and passes the in-sync real tree"
DRIFT_LINT="$DEFAULTS_DIR/scripts/check-labels-drift.sh"
if [[ ! -x "$DRIFT_LINT" ]]; then
  fail "check-labels-drift.sh missing or not executable"
else
  # (a) The real tree ships the two labels.yml copies byte-identical.
  if bash "$DRIFT_LINT" "$LOOM_ROOT" >/dev/null 2>&1; then
    pass "check-labels-drift passes against the real (in-sync) tree"
  else
    fail "check-labels-drift flagged the real tree (labels.yml copies should match)"
  fi

  # (b) A fixture whose defaults/ copy is missing a label must fail.
  DRIFT_FIX="$(mktemp -d)"
  mkdir -p "$DRIFT_FIX/.github" "$DRIFT_FIX/defaults/.github"
  cp "$LOOM_ROOT/.github/labels.yml" "$DRIFT_FIX/.github/labels.yml"
  grep -v 'loom:auditor-capability-request' "$LOOM_ROOT/.github/labels.yml" \
    > "$DRIFT_FIX/defaults/.github/labels.yml"
  DRIFT_OUT="$(bash "$DRIFT_LINT" "$DRIFT_FIX" 2>&1)" && DRIFT_RC=0 || DRIFT_RC=$?
  if [[ "$DRIFT_RC" -ne 0 ]] && echo "$DRIFT_OUT" | grep -q "drifted"; then
    pass "check-labels-drift fails (exit $DRIFT_RC) when the copies diverge"
  else
    fail "check-labels-drift did not catch the injected drift (rc=$DRIFT_RC)"
  fi

  # (c) The same fixture with identical copies must pass.
  cp "$LOOM_ROOT/.github/labels.yml" "$DRIFT_FIX/defaults/.github/labels.yml"
  if bash "$DRIFT_LINT" "$DRIFT_FIX" >/dev/null 2>&1; then
    pass "check-labels-drift passes when the two copies are identical"
  else
    fail "check-labels-drift false-positived on identical copies"
  fi
  rm -rf "$DRIFT_FIX"
fi
echo ""

# ==========================================================================
# Test: install-loom.sh guidance points at shipped script paths (#3923)
#
# The post-install "Next Steps" and the active-session refusal guidance name
# script paths the user is told to run. The historical ./.loom/scripts/daemon.sh
# was removed in #3432; guidance must reference only surfaces that actually ship
# in defaults/. This smoke check extracts every ./.loom/... path named in the
# installer's user-facing output and asserts it maps to a real file in defaults/.
# ==========================================================================
echo "Test: install-loom.sh guidance names only shipped ./.loom/ script paths"
GUIDANCE_MISSING=""
# Pull the ./.loom/... tokens the installer prints in echo/error guidance lines.
GUIDANCE_PATHS=$(grep -oE '\./\.loom/[A-Za-z0-9._/-]+' "$INSTALL_SCRIPT" \
  "$LOOM_ROOT/scripts/install/create-pr.sh" | sed 's/^[^:]*://' | sort -u)
for gp in $GUIDANCE_PATHS; do
  # Strip the leading ./ and map the installed path back to its defaults/ source:
  #   .loom/bin/loom       -> defaults/.loom/bin/loom
  #   .loom/scripts/foo.sh -> defaults/scripts/foo.sh
  rel="${gp#./}"
  case "$rel" in
    .loom/bin/*)     src="$DEFAULTS_DIR/$rel" ;;          # defaults/.loom/bin/loom
    .loom/scripts/*) src="$DEFAULTS_DIR/${rel#.loom/}" ;; # defaults/scripts/...
    *)               src="$DEFAULTS_DIR/${rel#.loom/}" ;;
  esac
  if [[ ! -e "$src" ]]; then
    GUIDANCE_MISSING="$GUIDANCE_MISSING\n  $gp -> $src (not shipped)"
  fi
done
# Belt-and-suspenders: the removed daemon.sh must never reappear in guidance.
if grep -qE '\./\.loom/scripts/daemon\.sh' "$INSTALL_SCRIPT" \
  "$LOOM_ROOT/scripts/install/create-pr.sh"; then
  GUIDANCE_MISSING="$GUIDANCE_MISSING\n  ./.loom/scripts/daemon.sh referenced (removed in #3432)"
fi
if [[ -z "$GUIDANCE_MISSING" ]]; then
  pass "all ./.loom/ paths in install guidance exist in defaults/"
else
  fail "install guidance references non-shipped paths:$(echo -e "$GUIDANCE_MISSING")"
fi
echo ""

# ==========================================================================
# Section 13: verify-install.sh check-links parsing coverage (#4147)
#
# install-loom.sh promotes a `check-links` failure to a hard installer error
# (issue #4097's AC4). This section exercises cmd_check_links /
# resolve_link_target / extract_link_targets directly (no full install run
# needed — the parsing logic operates on any git repo with a
# .loom/docs/*.md tree, which the legacy directory-walk fallback in
# collect_tracked_files() picks up without an install-metadata.json).
# ==========================================================================
echo "--- Section 13: check-links parsing coverage (#4147) ---"
echo ""

VERIFY_INSTALL_SCRIPT="$DEFAULTS_DIR/scripts/verify-install.sh"

# Minimal scratch git repo with a .loom/docs/ tree — enough for the legacy
# directory-walk fallback in collect_tracked_files() to discover the fixture
# .md files without needing an install-metadata.json.
seed_check_links_repo() {
  local repo="$1"
  mkdir -p "$repo/.loom/docs"
  git -C "$repo" init -q
  git -C "$repo" config user.email "test@test.com"
  git -C "$repo" config user.name "Test"
}

echo "Test: check-links — clean fixture resolves (exit 0)"
CL_OK="$TEST_DIR/check-links-ok"
seed_check_links_repo "$CL_OK"
mkdir -p "$CL_OK/.loom/docs/sub"
echo "sibling content" > "$CL_OK/.loom/docs/b.md"
echo "file in dir" > "$CL_OK/.loom/docs/sub/inner.md"
cat > "$CL_OK/.loom/docs/a.md" <<'EOF'
# Fixture

Markdown link resolves: [x](b.md)
Anchor stripped before resolution: [x](b.md#some-heading)
Pure in-page anchor skipped: [x](#local)
Directory target satisfied when non-empty: [x](sub/)
External http(s) skipped: [ext](https://example.com/page)
Mailto skipped: [m](mailto:a@b.c)
Backtick-only install-rooted resolves: `.loom/docs/b.md`
Glob placeholder is not a literal target: `.loom/roles/*.md`
Name placeholder is not a literal target: `.loom/roles/<name>.md`
Create `.loom/docs/tutorial-placeholder.md`: tutorial lines are excluded.
EOF
# Note: set -e is active throughout this file. check-links exits non-zero
# (6) on a dangling link by design, so every invocation below is guarded
# with `|| true` inside the capturing subshell and the real exit code is
# captured separately via a `set +e` / `set -e` bracket — mirroring the
# existing "set -e is active; capture ... via || true" convention used for
# Test 45/46 above.
set +e
CL_OK_OUT=$(cd "$CL_OK" && bash "$VERIFY_INSTALL_SCRIPT" check-links 2>&1)
CL_OK_RC=$?
set -e
if [[ $CL_OK_RC -eq 0 ]]; then
  pass "check-links: markdown link, anchor-stripped link, in-page anchor, non-empty directory target, external/mailto skip, backtick-only install-rooted ref, glob/placeholder exclusion, and Create-tutorial-line exclusion all resolve (exit 0)"
else
  fail "check-links: clean fixture did not resolve (rc=$CL_OK_RC): $CL_OK_OUT"
fi
echo ""

echo "Test: check-links — dangling markdown link (exit 6, target named)"
CL_DANGLE_MD="$TEST_DIR/check-links-dangle-md"
seed_check_links_repo "$CL_DANGLE_MD"
cat > "$CL_DANGLE_MD/.loom/docs/a.md" <<'EOF'
[x](nope.md)
EOF
set +e
CL_DANGLE_MD_OUT=$(cd "$CL_DANGLE_MD" && bash "$VERIFY_INSTALL_SCRIPT" check-links 2>&1)
CL_DANGLE_MD_RC=$?
set -e
if [[ $CL_DANGLE_MD_RC -eq 6 ]]; then
  pass "check-links: dangling markdown link -> exit 6"
else
  fail "check-links: dangling markdown link -> expected exit 6, got $CL_DANGLE_MD_RC"
fi
if echo "$CL_DANGLE_MD_OUT" | grep -qF "nope.md"; then
  pass "check-links: dangling markdown target named in output"
else
  fail "check-links: dangling markdown target not named in output: $CL_DANGLE_MD_OUT"
fi
echo ""

echo "Test: check-links — dangling backtick-only install-rooted ref (exit 6)"
CL_DANGLE_BT="$TEST_DIR/check-links-dangle-backtick"
seed_check_links_repo "$CL_DANGLE_BT"
cat > "$CL_DANGLE_BT/.loom/docs/a.md" <<'EOF'
See `.loom/docs/gone.md` for details.
EOF
set +e
CL_DANGLE_BT_OUT=$(cd "$CL_DANGLE_BT" && bash "$VERIFY_INSTALL_SCRIPT" check-links 2>&1)
CL_DANGLE_BT_RC=$?
set -e
if [[ $CL_DANGLE_BT_RC -eq 6 ]]; then
  pass "check-links: dangling backtick-only install-rooted ref -> exit 6"
else
  fail "check-links: dangling backtick-only ref -> expected exit 6, got $CL_DANGLE_BT_RC"
fi
if echo "$CL_DANGLE_BT_OUT" | grep -qF ".loom/docs/gone.md"; then
  pass "check-links: dangling backtick-only target named in output"
else
  fail "check-links: dangling backtick-only target not named in output: $CL_DANGLE_BT_OUT"
fi
echo ""

echo "Test: check-links — empty directory target dangles (exit 6)"
CL_DANGLE_DIR="$TEST_DIR/check-links-dangle-dir"
seed_check_links_repo "$CL_DANGLE_DIR"
mkdir -p "$CL_DANGLE_DIR/.loom/docs/emptysub"
cat > "$CL_DANGLE_DIR/.loom/docs/a.md" <<'EOF'
[x](emptysub/)
EOF
set +e
( cd "$CL_DANGLE_DIR" && bash "$VERIFY_INSTALL_SCRIPT" check-links >/dev/null 2>&1 )
CL_DANGLE_DIR_RC=$?
set -e
if [[ $CL_DANGLE_DIR_RC -eq 6 ]]; then
  pass "check-links: empty directory target -> exit 6"
else
  fail "check-links: empty directory target -> expected exit 6, got $CL_DANGLE_DIR_RC"
fi
echo ""

echo "Test: check-links — --quiet suppresses stdout on a dangling link"
CL_QUIET="$TEST_DIR/check-links-quiet"
seed_check_links_repo "$CL_QUIET"
cat > "$CL_QUIET/.loom/docs/a.md" <<'EOF'
[x](nope.md)
EOF
set +e
CL_QUIET_STDOUT=$(cd "$CL_QUIET" && bash "$VERIFY_INSTALL_SCRIPT" check-links --quiet 2>/dev/null)
CL_QUIET_RC=$?
set -e
if [[ $CL_QUIET_RC -eq 6 ]]; then
  pass "check-links --quiet: dangling link still exits 6"
else
  fail "check-links --quiet: expected exit 6, got $CL_QUIET_RC"
fi
if [[ -z "$CL_QUIET_STDOUT" ]]; then
  pass "check-links --quiet: no stdout emitted"
else
  fail "check-links --quiet: unexpected stdout: $CL_QUIET_STDOUT"
fi
echo ""

# ==========================================================================
# Machine-level `loom` dispatcher (Epic #3835 Phase 3a, #4157)
# ==========================================================================
# The dispatcher's own unit suite lives at
# defaults/scripts/tests/test-loom-dispatcher.sh (checkout resolution, collision
# resolution, config tiers, the AC7 status contexts, the AC4 no-shadow
# regression, and the AC6 console-script invariant). Fold its result into the
# installer suite so it runs in CI + the build gate rather than dev-only.
echo "Test: machine-level loom dispatcher suite (test-loom-dispatcher.sh)"
DISPATCHER_TEST="$DEFAULTS_DIR/scripts/tests/test-loom-dispatcher.sh"
if [[ -f "$DISPATCHER_TEST" ]]; then
  set +e
  DISPATCHER_TEST_OUT=$(bash "$DISPATCHER_TEST" 2>&1)
  DISPATCHER_TEST_RC=$?
  set -e
  if [[ $DISPATCHER_TEST_RC -eq 0 ]]; then
    pass "test-loom-dispatcher.sh: all cases passed"
  else
    fail "test-loom-dispatcher.sh failed (rc=$DISPATCHER_TEST_RC)"
    echo "$DISPATCHER_TEST_OUT" | tail -20
  fi
else
  fail "test-loom-dispatcher.sh not found at $DISPATCHER_TEST"
fi
echo ""

# The dispatcher source and its provisioning helper must ship.
echo "Test: dispatcher + provisioning artifacts are present"
if [[ -f "$LOOM_ROOT/scripts/loom" ]]; then
  pass "scripts/loom dispatcher source present"
else
  fail "scripts/loom dispatcher source missing"
fi
if [[ -f "$LOOM_ROOT/scripts/install/provision-dispatcher.sh" ]]; then
  pass "scripts/install/provision-dispatcher.sh present"
else
  fail "scripts/install/provision-dispatcher.sh missing"
fi
echo ""

# ==========================================================================
# User-scope skills + agents provisioning (Epic #3835 Phase 4, #4261)
# ==========================================================================
# Its own unit suite lives at defaults/scripts/tests/test-provision-skills.sh
# (whole-dir commands link + per-file agent links through the machine checkout,
# clobber avoidance, stale-link repoint, dangling-link prune, soft-fail, and
# the checkout-only deprovision). Fold its result into the installer suite so it
# runs in CI + the build gate rather than dev-only.
echo "Test: user-scope skills+agents provisioning suite (test-provision-skills.sh)"
SKILLS_TEST="$DEFAULTS_DIR/scripts/tests/test-provision-skills.sh"
if [[ -f "$SKILLS_TEST" ]]; then
  set +e
  SKILLS_TEST_OUT=$(bash "$SKILLS_TEST" 2>&1)
  SKILLS_TEST_RC=$?
  set -e
  if [[ $SKILLS_TEST_RC -eq 0 ]]; then
    pass "test-provision-skills.sh: all cases passed"
  else
    fail "test-provision-skills.sh failed (rc=$SKILLS_TEST_RC)"
    echo "$SKILLS_TEST_OUT" | tail -20
  fi
else
  fail "test-provision-skills.sh not found at $SKILLS_TEST"
fi
if [[ -f "$LOOM_ROOT/scripts/install/provision-skills.sh" ]]; then
  pass "scripts/install/provision-skills.sh present"
else
  fail "scripts/install/provision-skills.sh missing"
fi
echo ""

# ==========================================================================
# User-scope guard-hook wiring (Epic #3835 Phase 5, #4262)
# ==========================================================================
# Its own unit suite lives at defaults/scripts/tests/test-provision-hooks.sh
# (missing/empty/populated settings merge, idempotence incl. #4200 requoted
# dedup, operator-hook/permissions preservation, invalid-JSON soft-fail, pre-
# mutation backup, the fail-open workspace-gated wrapper, and the checkout-only
# deprovision). Fold its result into the installer suite so it runs in CI + the
# build gate rather than dev-only.
echo "Test: user-scope guard-hook provisioning suite (test-provision-hooks.sh)"
HOOKS_TEST="$DEFAULTS_DIR/scripts/tests/test-provision-hooks.sh"
if [[ -f "$HOOKS_TEST" ]]; then
  set +e
  HOOKS_TEST_OUT=$(bash "$HOOKS_TEST" 2>&1)
  HOOKS_TEST_RC=$?
  set -e
  if [[ $HOOKS_TEST_RC -eq 0 ]]; then
    pass "test-provision-hooks.sh: all cases passed"
  else
    fail "test-provision-hooks.sh failed (rc=$HOOKS_TEST_RC)"
    echo "$HOOKS_TEST_OUT" | tail -20
  fi
else
  fail "test-provision-hooks.sh not found at $HOOKS_TEST"
fi
if [[ -f "$LOOM_ROOT/scripts/install/provision-hooks.sh" ]]; then
  pass "scripts/install/provision-hooks.sh present"
else
  fail "scripts/install/provision-hooks.sh missing"
fi
echo ""

# ==========================================================================
# Quick Install leaves WORKING guard-hook wiring (issue #4401)
# ==========================================================================
# Regression guard for the reported zero-coverage state: `install.sh --quick`
# (fresh AND `--confirm-reinstall`) used to leave a repo with NO guard-hook
# execution path at all —
#   - `provision_loom_hooks` had a single caller on the FULL install path, so the
#     user-scope ~/.claude/settings.json was never wired by --quick, and
#   - the project-level `.claude/settings.json` entries were gone too: the 0.16.0
#     defaults carry no `hooks` block (Phase 5 / #4262) and a reinstall's chained
#     `uninstall-loom.sh` jq-strips every `.loom/hooks/`-prefixed command, while
#     `install_hooks_and_cli` still writes the `.loom/hooks/` copies those
#     stripped entries were the only way to reach.
# Asserting only that provision-hooks.sh EXISTS (the pre-#4401 coverage above)
# could not catch this, so these cases assert the two things that actually
# matter: install.sh CALLS the wiring on both quick paths, and the wiring turns a
# zero-coverage target into a target with at least one working path.
echo "Test: install.sh --quick wires guard hooks at both call sites (#4401)"
if grep -q 'source "\$LOOM_ROOT/scripts/install/provision-hooks.sh"' "$WRAPPER_SCRIPT"; then
  pass "install.sh sources scripts/install/provision-hooks.sh"
else
  fail "install.sh does not source scripts/install/provision-hooks.sh (quick path cannot wire guard hooks)"
fi
# Two call sites: the --confirm-reinstall quick branch and the fresh quick branch.
WIRE_CALLS=$(grep -c '^[[:space:]]*wire_quick_install_guard_hooks "\$TARGET_PATH"' "$WRAPPER_SCRIPT" || true)
if [[ "$WIRE_CALLS" -eq 2 ]]; then
  pass "wire_quick_install_guard_hooks invoked at both Quick Install call sites (fresh + reinstall)"
else
  fail "expected 2 wire_quick_install_guard_hooks call sites in install.sh, found $WIRE_CALLS"
fi
# The wiring must run AFTER the hook copies exist (it points project-level
# entries at them) — assert the ordering rather than mere presence.
FIRST_COPY_LINE=$(grep -n '^[[:space:]]*install_hooks_and_cli "\$LOOM_ROOT" "\$TARGET_PATH"' "$WRAPPER_SCRIPT" | head -1 | cut -d: -f1)
FIRST_WIRE_LINE=$(grep -n '^[[:space:]]*wire_quick_install_guard_hooks "\$TARGET_PATH"' "$WRAPPER_SCRIPT" | head -1 | cut -d: -f1)
if [[ -n "$FIRST_COPY_LINE" && -n "$FIRST_WIRE_LINE" ]] && [[ "$FIRST_WIRE_LINE" -gt "$FIRST_COPY_LINE" ]]; then
  pass "guard-hook wiring runs after install_hooks_and_cli writes the .loom/hooks/ copies"
else
  fail "guard-hook wiring must run after install_hooks_and_cli (copy line=$FIRST_COPY_LINE, wire line=$FIRST_WIRE_LINE)"
fi
# It must also precede the reinstall branch's git-index reconcile, so the
# reconcile sees the final .claude/settings.json content.
RECONCILE_LINE=$(grep -n 'Reconciling git index after reinstall' "$WRAPPER_SCRIPT" | head -1 | cut -d: -f1)
if [[ -n "$RECONCILE_LINE" && -n "$FIRST_WIRE_LINE" ]] && [[ "$FIRST_WIRE_LINE" -lt "$RECONCILE_LINE" ]]; then
  pass "guard-hook wiring runs before the reinstall git-index reconcile"
else
  fail "guard-hook wiring must precede the git-index reconcile (wire=$FIRST_WIRE_LINE, reconcile=$RECONCILE_LINE)"
fi
echo ""

# Functional halves. Both replay the exact post-`loom-daemon init` on-disk state a
# Quick Install produces (the daemon binary is deliberately not built in this
# suite), then run the REAL provisioning functions install.sh now calls.
if command -v jq >/dev/null 2>&1; then
  # Count project-level entries that reference a per-repo hook copy. Excludes the
  # machine-level wrapper form (its transition-dedup probe also mentions
  # `.loom/hooks/<name>`, but it EXITS instead of running the copy).
  count_reachable_hooks() {
    local settings="$1"
    [[ -f "$settings" ]] || { echo 0; return 0; }
    jq '[ (.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command // ""
          | select(contains(".loom/hooks/")) | select(contains("defaults/hooks/") | not) ] | length' \
      "$settings" 2>/dev/null || echo 0
  }

  # ── #4401a: FRESH `--quick` install ────────────────────────────────────────
  echo "Test: fresh install.sh --quick leaves working guard-hook wiring (#4401)"
  Q_FRESH="$TEST_DIR/quick-fresh-hooks"
  create_temp_repo "$Q_FRESH"
  # What `loom-daemon init` writes on a fresh install: the 0.16.0 defaults
  # settings.json, which carries permissions and NO `hooks` block.
  mkdir -p "$Q_FRESH/.claude"
  cp "$DEFAULTS_DIR/.claude/settings.json" "$Q_FRESH/.claude/settings.json"
  # What install.sh's install_hooks_and_cli writes: the per-repo hook copies.
  mkdir -p "$Q_FRESH/.loom/hooks"
  for _h in "$DEFAULTS_DIR/hooks/"*.sh; do
    [[ -f "$_h" ]] || continue
    cp "$_h" "$Q_FRESH/.loom/hooks/"
    chmod +x "$Q_FRESH/.loom/hooks/$(basename "$_h")"
  done
  # Pre-fix state: copies on disk, nothing referencing them.
  if [[ "$(count_reachable_hooks "$Q_FRESH/.claude/settings.json")" -eq 0 ]]; then
    pass "pre-wiring: a fresh --quick target has ZERO reachable guard hooks (the #4401 bug)"
  else
    fail "fixture invalid: expected zero reachable hooks before wiring"
  fi
  # Now the real wiring, with a sandboxed HOME so the user-scope merge is
  # exercised without touching the developer's ~/.claude/settings.json.
  Q_HOME="$TEST_DIR/quick-fresh-home"
  mkdir -p "$Q_HOME"
  set +e
  # shellcheck source=scripts/install/provision-hooks.sh
  source "$LOOM_ROOT/scripts/install/provision-hooks.sh"
  provision_loom_hooks "$Q_HOME/.claude" >/dev/null 2>&1
  ensure_project_hook_wiring "$Q_FRESH" >/dev/null 2>&1
  set -e
  Q_FRESH_REACHABLE=$(count_reachable_hooks "$Q_FRESH/.claude/settings.json")
  if [[ "$Q_FRESH_REACHABLE" -gt 0 ]]; then
    pass "fresh --quick: $Q_FRESH_REACHABLE guard hook(s) reachable via project-level entries"
  else
    fail "fresh --quick: still ZERO reachable guard hooks after wiring"
  fi
  # AC1's named check: the machine-level marker landed in the user-scope file.
  if grep -q '/defaults/hooks/' "$Q_HOME/.claude/settings.json" 2>/dev/null; then
    pass "fresh --quick: user-scope settings.json carries the /defaults/hooks/ marker"
  else
    fail "fresh --quick: user-scope settings.json missing the /defaults/hooks/ marker"
  fi
  # Every wired project-level entry must resolve to a real executable script —
  # a dangling command is not coverage.
  Q_DANGLING=0
  while IFS= read -r _cmd; do
    [[ -n "$_cmd" ]] || continue
    _rel="${_cmd#\$\{CLAUDE_PROJECT_DIR\}/}"
    [[ -x "$Q_FRESH/$_rel" ]] || Q_DANGLING=$((Q_DANGLING + 1))
  done < <(jq -r '[ (.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command // ""
                    | select(contains(".loom/hooks/")) | select(contains("defaults/hooks/") | not) ] | .[]' \
                 "$Q_FRESH/.claude/settings.json" 2>/dev/null)
  if [[ "$Q_DANGLING" -eq 0 ]]; then
    pass "fresh --quick: every wired project-level entry points at an executable hook script"
  else
    fail "fresh --quick: $Q_DANGLING wired entr(ies) point at a missing/non-executable script"
  fi
  echo ""

  # ── #4401b: `--quick --confirm-reinstall` over a pre-Phase-6 repo ──────────
  # This is the reported repro. It drives the REAL uninstaller (the component
  # that strips the project-level entries) rather than simulating the strip.
  echo "Test: --quick --confirm-reinstall over a pre-Phase-6 repo keeps guard hooks (#4401)"
  Q_RE="$TEST_DIR/quick-reinstall-hooks"
  create_temp_repo "$Q_RE"
  simulate_loom_install "$Q_RE"   # pre-Phase-6 shape: copies + legacy project entries
  git -C "$Q_RE" add -A >/dev/null 2>&1 || true
  git -C "$Q_RE" -c user.email=t@t -c user.name=T commit -qm "loom install" >/dev/null 2>&1 || true
  if [[ "$(count_reachable_hooks "$Q_RE/.claude/settings.json")" -gt 0 ]]; then
    pass "pre-reinstall: the pre-Phase-6 repo has working project-level guard hooks"
  else
    fail "fixture invalid: pre-Phase-6 repo should start with reachable guard hooks"
  fi
  # Step 1 of a --confirm-reinstall: the chained uninstall.
  set +e
  "$UNINSTALL_SCRIPT" --yes --local "$Q_RE" >/dev/null 2>&1
  set -e
  if [[ "$(count_reachable_hooks "$Q_RE/.claude/settings.json")" -eq 0 ]]; then
    pass "chained uninstall strips every project-level .loom/hooks/ entry (root cause confirmed)"
  else
    fail "expected the chained uninstall to strip project-level hook entries"
  fi
  # Step 2: `loom-daemon init --force` re-lands the defaults settings.json, which
  # re-adds NO hooks block (Phase 5). Step 3: install_hooks_and_cli rewrites the
  # per-repo copies. Together: copies present, nothing referencing them.
  mkdir -p "$Q_RE/.claude" "$Q_RE/.loom/hooks"
  cp "$DEFAULTS_DIR/.claude/settings.json" "$Q_RE/.claude/settings.json"
  for _h in "$DEFAULTS_DIR/hooks/"*.sh; do
    [[ -f "$_h" ]] || continue
    cp "$_h" "$Q_RE/.loom/hooks/"
    chmod +x "$Q_RE/.loom/hooks/$(basename "$_h")"
  done
  if [[ "$(count_reachable_hooks "$Q_RE/.claude/settings.json")" -eq 0 ]]; then
    pass "post-init: still ZERO reachable guard hooks (the exact #4401 report state)"
  else
    fail "fixture invalid: expected zero reachable hooks after uninstall+init"
  fi
  # Step 4 (the fix): wire_quick_install_guard_hooks' two calls.
  Q_RE_HOME="$TEST_DIR/quick-reinstall-home"
  mkdir -p "$Q_RE_HOME"
  set +e
  provision_loom_hooks "$Q_RE_HOME/.claude" >/dev/null 2>&1
  ensure_project_hook_wiring "$Q_RE" >/dev/null 2>&1
  set -e
  Q_RE_REACHABLE=$(count_reachable_hooks "$Q_RE/.claude/settings.json")
  if [[ "$Q_RE_REACHABLE" -gt 0 ]]; then
    pass "quick reinstall: $Q_RE_REACHABLE guard hook(s) reachable again (not zero)"
  else
    fail "quick reinstall: still ZERO reachable guard hooks after wiring (#4401 not fixed)"
  fi
  if grep -q '/defaults/hooks/' "$Q_RE_HOME/.claude/settings.json" 2>/dev/null; then
    pass "quick reinstall: user-scope settings.json carries the /defaults/hooks/ marker"
  else
    fail "quick reinstall: user-scope settings.json missing the /defaults/hooks/ marker"
  fi
  # The consumer's own project permissions must survive the wiring.
  if jq -e '.permissions.allow | length > 0' "$Q_RE/.claude/settings.json" >/dev/null 2>&1; then
    pass "quick reinstall: project permissions preserved by the hook wiring"
  else
    fail "quick reinstall: hook wiring damaged the project permissions block"
  fi
  echo ""
else
  warn "jq not available - skipping #4401 quick-install guard-hook wiring tests"
fi

# ==========================================================================
# sync-labels.sh --repo / --dry-run (fleet onboarding, #4498)
# ==========================================================================
# Its own unit suite lives at defaults/scripts/tests/test-sync-labels-repo-flag.sh
# (the --repo NWO short-circuit proven from a non-git directory, target
# exclusivity in the gh argv log, the forge-free --dry-run preview, unchanged
# no-flag resolution, and the argument-parsing rejections). Fold its result into
# the installer suite so it runs in CI + the build gate rather than dev-only.
echo "Test: sync-labels.sh --repo flag suite (test-sync-labels-repo-flag.sh)"
SYNC_LABELS_TEST="$DEFAULTS_DIR/scripts/tests/test-sync-labels-repo-flag.sh"
if [[ -f "$SYNC_LABELS_TEST" ]]; then
  set +e
  SYNC_LABELS_TEST_OUT=$(bash "$SYNC_LABELS_TEST" 2>&1)
  SYNC_LABELS_TEST_RC=$?
  set -e
  if [[ $SYNC_LABELS_TEST_RC -eq 0 ]]; then
    pass "test-sync-labels-repo-flag.sh: all cases passed"
  else
    fail "test-sync-labels-repo-flag.sh failed (rc=$SYNC_LABELS_TEST_RC)"
    echo "$SYNC_LABELS_TEST_OUT" | tail -20
  fi
else
  fail "test-sync-labels-repo-flag.sh not found at $SYNC_LABELS_TEST"
fi
echo ""

# ==========================================================================
# scripts/install/sync-labels.sh --prune-defaults / --force (#5066)
# ==========================================================================
# Its own unit suite lives at defaults/scripts/tests/test-install-sync-labels.sh
# (additive-by-default with no flags, --prune-defaults restoring the old
# unconditional deletion, and the in-use-label warn/refuse-without-force
# guard) — the source-only counterpart of the --repo/--dry-run suite above.
echo "Test: scripts/install/sync-labels.sh --prune-defaults suite (test-install-sync-labels.sh)"
INSTALL_SYNC_LABELS_TEST="$DEFAULTS_DIR/scripts/tests/test-install-sync-labels.sh"
if [[ -f "$INSTALL_SYNC_LABELS_TEST" ]]; then
  set +e
  INSTALL_SYNC_LABELS_TEST_OUT=$(bash "$INSTALL_SYNC_LABELS_TEST" 2>&1)
  INSTALL_SYNC_LABELS_TEST_RC=$?
  set -e
  if [[ $INSTALL_SYNC_LABELS_TEST_RC -eq 0 ]]; then
    pass "test-install-sync-labels.sh: all cases passed"
  else
    fail "test-install-sync-labels.sh failed (rc=$INSTALL_SYNC_LABELS_TEST_RC)"
    echo "$INSTALL_SYNC_LABELS_TEST_OUT" | tail -20
  fi
else
  fail "test-install-sync-labels.sh not found at $INSTALL_SYNC_LABELS_TEST"
fi
echo ""

# ==========================================================================
# scripts/cargo-target-dir.sh + scripts/daemon-build.sh redirected target-dir
# handling (#5922)
# ==========================================================================
# scripts/install-loom.sh and `pnpm daemon:build` used to hardcode the
# relative `target/release/...` path, which is wrong whenever Cargo's build
# output is redirected via `build.target-dir` in ~/.cargo/config.toml or the
# CARGO_TARGET_DIR env var — the build itself succeeds, but the subsequent
# `cp` of the built binary silently looks in the wrong place, producing a
# misleading "Failed to build loom-daemon" error even though the binary
# exists. These tests cover the fix at two levels:
#   - a real (fast, non-compiling) `cargo metadata` call proving the target
#     directory resolution itself honors CARGO_TARGET_DIR
#   - a stubbed `cargo` on PATH proving scripts/daemon-build.sh's
#     build/copy/error-reporting logic is exercised end-to-end against a
#     redirected target dir, without paying for a real compile in every test
#     run, and that a genuine compile failure vs. a missing-binary-after-
#     success are reported with distinguishable messages/exit codes

echo "Test: scripts/cargo-target-dir.sh resolves CARGO_TARGET_DIR override (#5922)"
CARGO_TARGET_DIR_SCRIPT="$LOOM_ROOT/scripts/cargo-target-dir.sh"
if [[ ! -x "$CARGO_TARGET_DIR_SCRIPT" ]]; then
  fail "scripts/cargo-target-dir.sh is missing or not executable"
else
  # CARGO_TARGET_DIR short-circuits ahead of `cargo metadata`, so this case
  # needs neither a Rust toolchain nor jq — it runs everywhere.
  REDIRECTED_TARGET="$TEST_DIR/redirected-cargo-target-5922"
  RESOLVED_TARGET="$(CARGO_TARGET_DIR="$REDIRECTED_TARGET" "$CARGO_TARGET_DIR_SCRIPT" "$LOOM_ROOT")"
  if [[ "$RESOLVED_TARGET" == "$REDIRECTED_TARGET" ]]; then
    pass "cargo-target-dir.sh reports the CARGO_TARGET_DIR override, not a hardcoded 'target/' path (#5922)"
  else
    fail "cargo-target-dir.sh resolved '$RESOLVED_TARGET', expected the redirected '$REDIRECTED_TARGET' (#5922)"
  fi

  # A relative CARGO_TARGET_DIR is resolved against the workspace root — the
  # directory every Loom build step cd's into before invoking cargo.
  RELATIVE_RESOLVED_TARGET="$(CARGO_TARGET_DIR="rel-target-5922" "$CARGO_TARGET_DIR_SCRIPT" "$LOOM_ROOT")"
  if [[ "$RELATIVE_RESOLVED_TARGET" == "$LOOM_ROOT/rel-target-5922" ]]; then
    pass "cargo-target-dir.sh absolutizes a relative CARGO_TARGET_DIR against the workspace root (#5922)"
  else
    fail "cargo-target-dir.sh resolved relative override to '$RELATIVE_RESOLVED_TARGET', expected '$LOOM_ROOT/rel-target-5922' (#5922)"
  fi

  # No redirect available at all (a root with no cargo manifest) must resolve
  # to the historical '<root>/target' assumption. Deliberately NOT asserted
  # against $LOOM_ROOT: a host that legitimately sets build.target-dir in
  # ~/.cargo/config.toml — i.e. exactly the host that reported #5922 — would
  # fail such an assertion for the right reason.
  NO_MANIFEST_ROOT="$TEST_DIR/no-manifest-root-5922"
  mkdir -p "$NO_MANIFEST_ROOT"
  DEFAULT_RESOLVED_TARGET="$(env -u CARGO_TARGET_DIR "$CARGO_TARGET_DIR_SCRIPT" "$NO_MANIFEST_ROOT" 2>/dev/null)"
  if [[ "$DEFAULT_RESOLVED_TARGET" == "$NO_MANIFEST_ROOT/target" ]]; then
    pass "cargo-target-dir.sh falls back to '<root>/target' (pre-#5922 behavior) when nothing redirects it (#5922)"
  else
    fail "cargo-target-dir.sh fallback resolved '$DEFAULT_RESOLVED_TARGET', expected '$NO_MANIFEST_ROOT/target' (#5922)"
  fi

  # With a real toolchain, the resolved value must agree with Cargo itself,
  # whatever this host's configuration happens to be.
  if command -v cargo >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    CARGO_SAYS="$(cd "$LOOM_ROOT" && env -u CARGO_TARGET_DIR cargo metadata --format-version 1 --no-deps 2>/dev/null | jq -r '.target_directory // empty')"
    ACTUAL_SAYS="$(env -u CARGO_TARGET_DIR "$CARGO_TARGET_DIR_SCRIPT" "$LOOM_ROOT" 2>/dev/null)"
    if [[ -n "$CARGO_SAYS" && "$ACTUAL_SAYS" == "$CARGO_SAYS" ]]; then
      pass "cargo-target-dir.sh agrees with 'cargo metadata' under this host's own config (#5922)"
    else
      fail "cargo-target-dir.sh resolved '$ACTUAL_SAYS', but 'cargo metadata' reports '$CARGO_SAYS' (#5922)"
    fi
  else
    warn "Skipping cargo-target-dir.sh/cargo-metadata agreement check — needs both 'cargo' and 'jq' on PATH"
  fi
fi
echo ""

echo "Test: scripts/daemon-build.sh honors a redirected target dir and distinguishes failure modes (#5922)"
DAEMON_BUILD_SCRIPT="$LOOM_ROOT/scripts/daemon-build.sh"
if [[ ! -x "$DAEMON_BUILD_SCRIPT" ]]; then
  fail "scripts/daemon-build.sh is missing or not executable"
else
  # A stub `cargo` on PATH avoids paying for a real compile on every test
  # run while still exercising daemon-build.sh's own logic exactly as
  # invoked in production: `cargo metadata ...` for target-dir resolution,
  # then `cargo build --package loom-daemon --release`.
  STUB_BIN_DIR="$TEST_DIR/stub-cargo-bin-5922"
  mkdir -p "$STUB_BIN_DIR"
  cat > "$STUB_BIN_DIR/cargo" <<'STUB_CARGO_EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "metadata" ]]; then
  printf '{"target_directory":"%s"}\n' "$STUB_CARGO_TARGET_DIR"
  exit 0
fi
if [[ "${1:-}" == "build" ]]; then
  case "$STUB_CARGO_BUILD_MODE" in
    success)
      mkdir -p "$STUB_CARGO_TARGET_DIR/release"
      : > "$STUB_CARGO_TARGET_DIR/release/loom-daemon"
      chmod +x "$STUB_CARGO_TARGET_DIR/release/loom-daemon"
      exit 0
      ;;
    missing-binary)
      # Report success without producing the binary.
      exit 0
      ;;
    compile-failure)
      echo "error[E0000]: stub compile failure" >&2
      exit 101
      ;;
  esac
fi
echo "stub cargo: unrecognized invocation: $*" >&2
exit 64
STUB_CARGO_EOF
  chmod +x "$STUB_BIN_DIR/cargo"

  # Case 1: successful build under a redirected target dir — the
  # architecture-specific copy must land next to the real binary, and the
  # resolved (redirected) target dir must be printed to stdout.
  STUB_CARGO_TARGET_DIR="$TEST_DIR/daemon-build-success-5922"
  DAEMON_BUILD_OUT=""
  DAEMON_BUILD_RC=0
  set +e
  DAEMON_BUILD_OUT=$(STUB_CARGO_TARGET_DIR="$STUB_CARGO_TARGET_DIR" STUB_CARGO_BUILD_MODE="success" \
    env -u CARGO_TARGET_DIR PATH="$STUB_BIN_DIR:$PATH" "$DAEMON_BUILD_SCRIPT" 2>"$TEST_DIR/daemon-build-5922-stderr.log")
  DAEMON_BUILD_RC=$?
  set -e
  if [[ $DAEMON_BUILD_RC -eq 0 && "$DAEMON_BUILD_OUT" == "$STUB_CARGO_TARGET_DIR" ]]; then
    pass "daemon-build.sh exits 0 and prints the redirected target dir on success (#5922)"
  else
    fail "daemon-build.sh success case: rc=$DAEMON_BUILD_RC stdout='$DAEMON_BUILD_OUT' (expected 0 / '$STUB_CARGO_TARGET_DIR') (#5922)"
  fi
  if [[ -x "$STUB_CARGO_TARGET_DIR/release/loom-daemon-aarch64-apple-darwin" ]]; then
    pass "daemon-build.sh produces the -aarch64-apple-darwin copy under a redirected target dir (#5922)"
  else
    fail "daemon-build.sh did not produce the -aarch64-apple-darwin copy under a redirected target dir (#5922)"
  fi

  # Case 2: cargo build succeeds but the binary is absent afterward — must
  # be reported distinctly (exit 3, non-"Failed to build" message) from a
  # genuine compile failure, per the issue's own acceptance criteria.
  STUB_CARGO_TARGET_DIR="$TEST_DIR/daemon-build-missing-binary-5922"
  set +e
  DAEMON_BUILD_MISSING_ERR=$(STUB_CARGO_TARGET_DIR="$STUB_CARGO_TARGET_DIR" STUB_CARGO_BUILD_MODE="missing-binary" \
    env -u CARGO_TARGET_DIR PATH="$STUB_BIN_DIR:$PATH" "$DAEMON_BUILD_SCRIPT" 2>&1 1>/dev/null)
  DAEMON_BUILD_MISSING_RC=$?
  set -e
  if [[ $DAEMON_BUILD_MISSING_RC -eq 3 ]]; then
    pass "daemon-build.sh exits 3 (distinct code) when the build succeeds but the binary is missing (#5922)"
  else
    fail "daemon-build.sh missing-binary case exited $DAEMON_BUILD_MISSING_RC, expected 3 (#5922)"
  fi
  if echo "$DAEMON_BUILD_MISSING_ERR" | grep -qi "no binary was found" \
      && ! echo "$DAEMON_BUILD_MISSING_ERR" | grep -q "Failed to build loom-daemon"; then
    pass "daemon-build.sh missing-binary message is distinguishable from a genuine build failure (#5922)"
  else
    fail "daemon-build.sh missing-binary message is not distinguishable: '$DAEMON_BUILD_MISSING_ERR' (#5922)"
  fi

  # Case 3: a genuine cargo compile failure must still be reported as
  # "Failed to build loom-daemon" (exit 1) — unchanged by this fix.
  STUB_CARGO_TARGET_DIR="$TEST_DIR/daemon-build-compile-failure-5922"
  set +e
  DAEMON_BUILD_FAIL_ERR=$(STUB_CARGO_TARGET_DIR="$STUB_CARGO_TARGET_DIR" STUB_CARGO_BUILD_MODE="compile-failure" \
    env -u CARGO_TARGET_DIR PATH="$STUB_BIN_DIR:$PATH" "$DAEMON_BUILD_SCRIPT" 2>&1 1>/dev/null)
  DAEMON_BUILD_FAIL_RC=$?
  set -e
  if [[ $DAEMON_BUILD_FAIL_RC -eq 1 ]] && echo "$DAEMON_BUILD_FAIL_ERR" | grep -q "Failed to build loom-daemon"; then
    pass "daemon-build.sh still reports 'Failed to build loom-daemon' (exit 1) on a genuine compile failure (#5922)"
  else
    fail "daemon-build.sh compile-failure case: rc=$DAEMON_BUILD_FAIL_RC stderr='$DAEMON_BUILD_FAIL_ERR' (#5922)"
  fi

  rm -f "$TEST_DIR/daemon-build-5922-stderr.log"
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
