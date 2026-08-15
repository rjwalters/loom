#!/usr/bin/env bash
# Test suite for install.sh::print_what_will_be_installed() (#6330).
#
# Usage: ./tests/install/test-dry-run-preview-manifest.sh
#
# Background: the "What Will Be Installed" preview (shown both by `--dry-run`
# and before the real pre-install confirmation prompt) used to be a
# hand-maintained, hardcoded list that silently fell out of sync with the
# files Loom actually installs (missing AGENTS.md, loom.sh, package.json,
# .gitattributes, .claude/agents/, .claude/README.md, .claude/biome.jsonc; and
# never mentioning that .claude/settings.json is modified in place). The fix
# renders the bulk of the preview from _emit_loom_ownership_set()
# (scripts/install/manifest.sh) -- the SAME defaults/-walk that produces
# .loom/install-metadata.json's installed_files array -- so it cannot drift
# from the real file list again.
#
# install.sh runs top-level installer logic when sourced, so we extract just
# the print_what_will_be_installed() function definition (same awk-to-
# closing-brace technique as test-hooks-preserve.sh /
# test-manifest-freshness-after-hook-wiring.sh) and eval it in isolation,
# against the REAL defaults/ tree (LOOM_ROOT=$REPO_ROOT) and synthetic
# TARGET_PATH scratch directories.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"
MANIFEST_SH="$REPO_ROOT/scripts/install/manifest.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0
TOTAL=0

assert_contains() {
  local desc="$1" haystack="$2" needle="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" == *"$needle"* ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected to find: '$needle'"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local desc="$1" haystack="$2" needle="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" != *"$needle"* ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected NOT to find: '$needle'"
    FAIL=$((FAIL + 1))
  fi
}

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

# Stub logging helpers so the extracted function has them in scope. Unlike
# install.sh's real versions (which prefix ANSI color codes we don't care
# about here), these still echo their message -- print_what_will_be_installed()
# puts section headers (including the manifest-derived file counts we assert
# on below) through info()/warning()/header(), so a silent no-op stub would
# swallow the very text this suite needs to inspect.
header()  { echo "$*"; }
info()    { echo "$*"; }
success() { echo "$*"; }
warning() { echo "$*"; }

if [[ ! -f "$MANIFEST_SH" ]]; then
  echo -e "${RED}FATAL${NC}: manifest.sh not found at $MANIFEST_SH"
  exit 1
fi
# shellcheck source=/dev/null
source "$MANIFEST_SH"

_FN_SRC="$(awk '/^print_what_will_be_installed\(\) \{/{f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
if [[ -z "$_FN_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract print_what_will_be_installed() from $INSTALL_SH"
  exit 1
fi
eval "$_FN_SRC"

# Real Loom source checkout -- defaults/ must exist for the manifest walk to
# produce a non-empty list.
LOOM_ROOT="$REPO_ROOT"

# ============================================================================
# Test 1: a fresh target (no package.json, not the Loom source repo) lists
# every root-level file the real install creates, per issue #6330's AC.
# ============================================================================
echo ""
echo "=== Fresh target: root-level files and manifest-driven sections ==="

TARGET1="$(mktemp -d)"
mkdir -p "$TARGET1/.git"

OUT1="$(TARGET_PATH="$TARGET1" print_what_will_be_installed)"

for f in "CLAUDE.md" "AGENTS.md" "loom.sh" "package.json" ".gitattributes" ".claude/settings.json"; do
  assert_contains "fresh-target preview mentions $f" "$OUT1" "$f"
done

# Modifications section must list more than just .gitignore (the original bug).
assert_contains "fresh-target preview lists .gitignore as a modification" "$OUT1" ".gitignore"
assert_contains "fresh-target preview lists .claude/settings.json as a modification" "$OUT1" "project-level guard-hook entries wired in"

rm -rf "$TARGET1"

# ============================================================================
# Test 2: a target that already has its own package.json -- the preview must
# NOT claim package.json will be created (mirrors _emit_installed_files_manifest's
# own conditional-presence rule).
# ============================================================================
echo ""
echo "=== Target with existing package.json: conditional presence ==="

TARGET2="$(mktemp -d)"
mkdir -p "$TARGET2/.git"
echo '{}' > "$TARGET2/package.json"

OUT2="$(TARGET_PATH="$TARGET2" print_what_will_be_installed)"
assert_not_contains "preview omits package.json bullet when target already has one" "$OUT2" "• package.json"
# Sanity: other root files are still listed.
assert_contains "preview still lists CLAUDE.md when package.json is skipped" "$OUT2" "CLAUDE.md"

rm -rf "$TARGET2"

# ============================================================================
# Test 3: the preview's manifest-derived counts match
# _emit_installed_files_manifest()'s own output exactly -- the drift-proofing
# property issue #6330 asks for.
# ============================================================================
echo ""
echo "=== Preview counts match _emit_loom_ownership_set() directly ==="

TARGET3="$(mktemp -d)"
mkdir -p "$TARGET3/.git"

OUT3="$(TARGET_PATH="$TARGET3" print_what_will_be_installed)"

LIVE_MANIFEST="$(LOOM_ROOT="$REPO_ROOT" TARGET_PATH="$TARGET3" DOGFOOD_MODE="false" _emit_loom_ownership_set)"
LIVE_LOOM_COUNT="$(printf '%s\n' "$LIVE_MANIFEST" | grep -c '^\.loom/')"
LIVE_CLAUDE_COUNT="$(printf '%s\n' "$LIVE_MANIFEST" | grep -c '^\.claude/')"
LIVE_GITHUB_COUNT="$(printf '%s\n' "$LIVE_MANIFEST" | grep -c '^\.github/')"

PREVIEW_LOOM_COUNT="$(printf '%s\n' "$OUT3" | grep -oE '\.loom/, committed to git, [0-9]+ files' | grep -oE '[0-9]+')"
PREVIEW_CLAUDE_COUNT="$(printf '%s\n' "$OUT3" | grep -oE '\.claude/, committed to git, [0-9]+ files' | grep -oE '[0-9]+')"
PREVIEW_GITHUB_COUNT="$(printf '%s\n' "$OUT3" | grep -oE '\.github/, committed to git, [0-9]+ files' | grep -oE '[0-9]+')"

assert_eq "preview .loom/ count matches _emit_loom_ownership_set" "$LIVE_LOOM_COUNT" "$PREVIEW_LOOM_COUNT"
assert_eq "preview .claude/ count matches _emit_loom_ownership_set" "$LIVE_CLAUDE_COUNT" "$PREVIEW_CLAUDE_COUNT"
assert_eq "preview .github/ count matches _emit_loom_ownership_set" "$LIVE_GITHUB_COUNT" "$PREVIEW_GITHUB_COUNT"

rm -rf "$TARGET3"

# ============================================================================
# Test 4: dogfood mode excludes .claude/agents/* from both the live manifest
# and the rendered preview's .claude/ count, and the preview's wording
# switches to the symlink note (mirrors scripts/install-loom.sh's TARGET_PATH
# == LOOM_ROOT auto-detect, see #3311).
# ============================================================================
echo ""
echo "=== Dogfood mode: .claude/agents/* excluded from the preview ==="

LIVE_MANIFEST_DOGFOOD="$(LOOM_ROOT="$REPO_ROOT" TARGET_PATH="$REPO_ROOT" DOGFOOD_MODE="true" _emit_loom_ownership_set)"
LIVE_CLAUDE_COUNT_DOGFOOD="$(printf '%s\n' "$LIVE_MANIFEST_DOGFOOD" | grep -c '^\.claude/')"

AGENTS_FILE_COUNT="$(find "$REPO_ROOT/defaults/.claude/agents" -type f | wc -l | tr -d ' ')"
TOTAL=$((TOTAL + 1))
if [[ "$LIVE_CLAUDE_COUNT_DOGFOOD" -lt "$LIVE_CLAUDE_COUNT" ]] && \
   [[ $((LIVE_CLAUDE_COUNT - LIVE_CLAUDE_COUNT_DOGFOOD)) -eq "$AGENTS_FILE_COUNT" ]]; then
  echo -e "${GREEN}PASS${NC}: dogfood mode excludes exactly the $AGENTS_FILE_COUNT files under defaults/.claude/agents/"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: dogfood mode excludes exactly the $AGENTS_FILE_COUNT files under defaults/.claude/agents/"
  echo "  non-dogfood .claude/ count: $LIVE_CLAUDE_COUNT"
  echo "  dogfood .claude/ count:     $LIVE_CLAUDE_COUNT_DOGFOOD"
  FAIL=$((FAIL + 1))
fi

# Drive the extracted function itself in dogfood mode (TARGET_PATH == LOOM_ROOT
# is print_what_will_be_installed's own auto-detect condition) and confirm its
# rendered .claude/ count and wording match.
OUT4="$(TARGET_PATH="$REPO_ROOT" print_what_will_be_installed)"
PREVIEW_CLAUDE_COUNT_DOGFOOD="$(printf '%s\n' "$OUT4" | grep -oE '\.claude/, committed to git, [0-9]+ files' | grep -oE '[0-9]+')"
assert_eq "preview .claude/ count matches dogfood-mode manifest" "$LIVE_CLAUDE_COUNT_DOGFOOD" "$PREVIEW_CLAUDE_COUNT_DOGFOOD"
assert_contains "preview notes the dogfood-mode symlink instead of a plain copy" "$OUT4" "Symlinked (not copied)"

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
