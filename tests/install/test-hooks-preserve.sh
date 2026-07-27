#!/usr/bin/env bash
# Test suite for install.sh::install_hooks_and_cli() hook preservation (#3625).
#
# Usage: ./tests/install/test-hooks-preserve.sh
#
# Verifies that the quick-install path does NOT silently clobber an existing
# (possibly downstream-tuned/forked) hook — most importantly a customized
# guard-destructive.sh with a hand-tuned rm allowlist. Preservation mirrors the
# skip-unless-force behavior in scripts/install-loom.sh:1099-1116; the previous
# install.sh implementation did an unconditional cp overwrite.
#
# install.sh runs top-level installer logic when sourced, so we extract just the
# install_hooks_and_cli() function definition and eval it in isolation with stub
# logging helpers.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

PASS=0
FAIL=0
TOTAL=0

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

# Stub logging helpers so the extracted function has them in scope.
info()    { :; }
success() { :; }
warning() { :; }
error()   { echo "error: $*" >&2; return 1; }

# Extract the install_hooks_and_cli() function body from install.sh and define
# it here. awk grabs from the function header to the first closing brace at
# column 0.
_FN_SRC="$(awk '/^install_hooks_and_cli\(\) \{/{f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
if [[ -z "$_FN_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract install_hooks_and_cli() from $INSTALL_SH"
  exit 1
fi
eval "$_FN_SRC"

# install_hooks_and_cli() calls canonical_guard_present() (#4041) — extract and
# define it too so the isolated function has its dependency in scope.
_HELPER_SRC="$(awk '/^canonical_guard_present\(\) \{/{f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
if [[ -z "$_HELPER_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract canonical_guard_present() from $INSTALL_SH"
  exit 1
fi
eval "$_HELPER_SRC"

# Build a fake loom_root whose defaults/hooks ships a canonical guard hook (the
# dispatcher stand-in) plus the vendored generic guard (#4041).
make_loom_root() {
  local root="$1"
  mkdir -p "$root/defaults/hooks"
  printf '%s\n' '#!/usr/bin/env bash' '# CANONICAL guard-destructive.sh' > \
    "$root/defaults/hooks/guard-destructive.sh"
  printf '%s\n' '#!/usr/bin/env bash' '# VENDORED-GENERIC guard' > \
    "$root/defaults/hooks/guard-destructive-generic.sh"
}

# Install the canonical Repo Skills guard (with the repo#29 marker) into a target
# so canonical_guard_present() returns true for it.
install_canonical_repo_skills_guard() {
  local target="$1"
  mkdir -p "$target/.claude/skills/repo/hooks"
  printf '%s\n' '#!/usr/bin/env bash' '# canonical, carries repo#29 fix' > \
    "$target/.claude/skills/repo/hooks/guard-destructive.sh"
}

TUNED_MARKER='# TUNED-FORK: downstream rm allowlist'

# ============================================================================
# Test 1: existing tuned hook is preserved (no force)
# ============================================================================
echo ""
echo "=== install_hooks_and_cli preserves an existing tuned hook ==="

LOOM_ROOT_DIR="$(mktemp -d)"
TARGET_DIR="$(mktemp -d)"
trap 'rm -rf "$LOOM_ROOT_DIR" "$TARGET_DIR"' EXIT
make_loom_root "$LOOM_ROOT_DIR"

mkdir -p "$TARGET_DIR/.loom/hooks"
printf '%s\n' '#!/usr/bin/env bash' "$TUNED_MARKER" > \
  "$TARGET_DIR/.loom/hooks/guard-destructive.sh"

install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR"

assert_eq "tuned hook preserved (default, no force)" \
  "yes" \
  "$(grep -qF "$TUNED_MARKER" "$TARGET_DIR/.loom/hooks/guard-destructive.sh" && echo yes || echo no)"

# ============================================================================
# Test 2: existing tuned hook IS overwritten when force=true
# ============================================================================
echo ""
echo "=== install_hooks_and_cli overwrites when force=true ==="

TARGET_DIR2="$(mktemp -d)"
mkdir -p "$TARGET_DIR2/.loom/hooks"
printf '%s\n' '#!/usr/bin/env bash' "$TUNED_MARKER" > \
  "$TARGET_DIR2/.loom/hooks/guard-destructive.sh"

install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR2" "true"

assert_eq "tuned hook overwritten with force=true" \
  "no" \
  "$(grep -qF "$TUNED_MARKER" "$TARGET_DIR2/.loom/hooks/guard-destructive.sh" && echo yes || echo no)"
assert_eq "canonical hook installed with force=true" \
  "yes" \
  "$(grep -qF 'CANONICAL' "$TARGET_DIR2/.loom/hooks/guard-destructive.sh" && echo yes || echo no)"
rm -rf "$TARGET_DIR2"

# ============================================================================
# Test 3: fresh target (no existing hook) installs the canonical hook
# ============================================================================
echo ""
echo "=== install_hooks_and_cli installs on a fresh target ==="

TARGET_DIR3="$(mktemp -d)"
install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR3"

assert_eq "hook installed on fresh target" \
  "yes" \
  "$([[ -f "$TARGET_DIR3/.loom/hooks/guard-destructive.sh" ]] && grep -qF 'CANONICAL' "$TARGET_DIR3/.loom/hooks/guard-destructive.sh" && echo yes || echo no)"
assert_eq "installed hook is executable" \
  "yes" \
  "$([[ -x "$TARGET_DIR3/.loom/hooks/guard-destructive.sh" ]] && echo yes || echo no)"
rm -rf "$TARGET_DIR3"

# ============================================================================
# Test 4 (#4041): canonical guard present → vendored generic NOT installed,
# but the dispatcher (guard-destructive.sh) IS installed.
# ============================================================================
echo ""
echo "=== canonical present: vendored generic skipped, dispatcher installed ==="

TARGET_DIR4="$(mktemp -d)"
install_canonical_repo_skills_guard "$TARGET_DIR4"
install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR4"

assert_eq "dispatcher (guard-destructive.sh) installed when canonical present" \
  "yes" \
  "$([[ -f "$TARGET_DIR4/.loom/hooks/guard-destructive.sh" ]] && echo yes || echo no)"
assert_eq "vendored generic NOT installed when canonical present" \
  "no" \
  "$([[ -f "$TARGET_DIR4/.loom/hooks/guard-destructive-generic.sh" ]] && echo yes || echo no)"
rm -rf "$TARGET_DIR4"

# ============================================================================
# Test 5 (#4041): canonical guard present → a STALE vendored generic from a
# prior install is removed (converge to "canonical only").
# ============================================================================
echo ""
echo "=== canonical present: stale vendored generic removed ==="

TARGET_DIR5="$(mktemp -d)"
install_canonical_repo_skills_guard "$TARGET_DIR5"
mkdir -p "$TARGET_DIR5/.loom/hooks"
printf '%s\n' '#!/usr/bin/env bash' '# STALE vendored generic' > \
  "$TARGET_DIR5/.loom/hooks/guard-destructive-generic.sh"

install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR5"

assert_eq "stale vendored generic removed when canonical present" \
  "no" \
  "$([[ -f "$TARGET_DIR5/.loom/hooks/guard-destructive-generic.sh" ]] && echo yes || echo no)"
rm -rf "$TARGET_DIR5"

# ============================================================================
# Test 6 (#4041): canonical guard ABSENT → vendored generic IS installed
# (standalone-Loom repo keeps generic coverage).
# ============================================================================
echo ""
echo "=== canonical absent: vendored generic installed ==="

TARGET_DIR6="$(mktemp -d)"
install_hooks_and_cli "$LOOM_ROOT_DIR" "$TARGET_DIR6"

assert_eq "vendored generic installed when canonical absent" \
  "yes" \
  "$([[ -f "$TARGET_DIR6/.loom/hooks/guard-destructive-generic.sh" ]] && echo yes || echo no)"
rm -rf "$TARGET_DIR6"

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
