#!/usr/bin/env bash
# Test suite for install.sh::regenerate_manifest_after_hook_wiring() (#5279).
#
# Usage: ./tests/install/test-manifest-freshness-after-hook-wiring.sh
#
# Background: `loom-daemon init` generates `.loom/manifest.json` as its own
# last internal step -- BEFORE `wire_quick_install_guard_hooks` (install.sh)
# asserts project-level guard-hook entries into `.claude/settings.json`. That
# means the manifest's stored checksum for `.claude/settings.json` reflected
# PRE-wiring content, so `verify-install.sh verify` reported spurious
# `DRIFT DETECTED` on that file after EVERY quick install -- even a
# completely vanilla install with zero customization -- because the on-disk
# file legitimately changed after the manifest snapshot was taken.
#
# `regenerate_manifest_after_hook_wiring()` fixes this by re-running
# `verify-install.sh generate --quiet` after all `.claude/settings.json`
# mutations for the install path are complete. This suite extracts just that
# function (install.sh runs top-level installer logic when sourced, so we use
# the same awk-extraction + eval technique as test-hooks-preserve.sh) and
# drives it against a real (copied-in) verify-install.sh + a synthetic
# install-metadata.json, with no loom-daemon binary or cargo build required.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"
VERIFY_INSTALL_SH="$REPO_ROOT/defaults/scripts/verify-install.sh"

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

# Stub logging helpers so the extracted function has them in scope.
info()    { :; }
success() { :; }
warning() { echo "warning: $*" >&2; }
error()   { echo "error: $*" >&2; return 1; }

# Extract regenerate_manifest_after_hook_wiring() from install.sh (same
# awk-to-closing-brace technique as test-hooks-preserve.sh /
# test-daemon-build-shortcut.sh).
_FN_SRC="$(awk '/^regenerate_manifest_after_hook_wiring\(\) \{/{f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
if [[ -z "$_FN_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract regenerate_manifest_after_hook_wiring() from $INSTALL_SH"
  exit 1
fi
eval "$_FN_SRC"

if [[ ! -f "$VERIFY_INSTALL_SH" ]]; then
  echo -e "${RED}FATAL${NC}: verify-install.sh not found at $VERIFY_INSTALL_SH"
  exit 1
fi

# Build a minimal synthetic "installed" target: a git repo carrying just
# .claude/settings.json as the sole Loom-tracked file (via a synthetic
# install-metadata.json), plus a real copy of verify-install.sh so the
# extracted function has something real to invoke.
make_target() {
  local target="$1"
  mkdir -p "$target/.git" "$target/.claude" "$target/.loom/scripts"
  cp "$VERIFY_INSTALL_SH" "$target/.loom/scripts/verify-install.sh"
  chmod +x "$target/.loom/scripts/verify-install.sh"
  cat > "$target/.loom/install-metadata.json" <<'EOF'
{"installed_files": [".claude/settings.json"]}
EOF
}

# ============================================================================
# Test 1: manifest is regenerated to match settings.json mutated AFTER the
# initial `loom-daemon init` manifest snapshot (simulated here by generating
# once, then mutating the file, then calling the function under test).
# ============================================================================
echo ""
echo "=== regenerate_manifest_after_hook_wiring refreshes a stale settings.json hash ==="

TARGET1="$(mktemp -d)"
make_target "$TARGET1"

printf '%s\n' '{"permissions": {"allow": ["Bash(gh:*)"]}}' > "$TARGET1/.claude/settings.json"

# Simulate loom-daemon init's own internal manifest generation (BEFORE the
# hook-wiring mutation below) -- this is the snapshot that used to go stale.
( cd "$TARGET1" && bash .loom/scripts/verify-install.sh generate --quiet )

# Simulate wire_quick_install_guard_hooks mutating settings.json AFTER that
# snapshot (e.g. ensure_project_hook_wiring asserting project-level hooks).
printf '%s\n' \
  '{"permissions": {"allow": ["Bash(gh:*)"]}, "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]}]}}' \
  > "$TARGET1/.claude/settings.json"

# Before the fix: verify would report drift here, since the manifest was
# generated against the pre-mutation content.
PRE_STATUS=$(cd "$TARGET1" && bash .loom/scripts/verify-install.sh verify --json 2>/dev/null | jq -r '.status')
assert_eq "sanity: verify reports drift before regeneration" "drift" "$PRE_STATUS"

regenerate_manifest_after_hook_wiring "$TARGET1"

POST_STATUS=$(cd "$TARGET1" && bash .loom/scripts/verify-install.sh verify --json 2>/dev/null | jq -r '.status')
assert_eq "verify reports ok after regenerate_manifest_after_hook_wiring" "ok" "$POST_STATUS"

rm -rf "$TARGET1"

# ============================================================================
# Test 2: a target with no .loom/scripts/verify-install.sh at all is a silent
# best-effort no-op (never aborts the install).
# ============================================================================
echo ""
echo "=== regenerate_manifest_after_hook_wiring no-ops cleanly when verify-install.sh is absent ==="

TARGET2="$(mktemp -d)"
mkdir -p "$TARGET2/.git" "$TARGET2/.claude"

set +e
regenerate_manifest_after_hook_wiring "$TARGET2"
RC=$?
set -e
assert_eq "missing verify-install.sh does not abort (best-effort)" "0" "$RC"

rm -rf "$TARGET2"

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
