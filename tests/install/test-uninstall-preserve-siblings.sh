#!/usr/bin/env bash
# Test suite for uninstall-loom.sh Step 7 "Clean Up Empty Directories" (#3634).
#
# Usage: ./tests/install/test-uninstall-preserve-siblings.sh
#
# Regression: the emptiness predicate used `find "$dir" -type f`, which does NOT
# count symlinks (type `l`) or sub-directories. A co-installed tool (e.g. Repo
# Skills) installs `.claude/commands/repo/` as a directory of *symlinks*. After
# Loom removed its own files, `.claude/commands/` contained only that foreign
# symlink subdir; `find -type f` reported it "empty" and `rm -rf` clobbered a
# DIFFERENT tool's install. The fix uses `-mindepth 1 -not -name '.DS_Store'`,
# which counts symlinks/subdirs so foreign content is left alone while genuinely
# empty Loom-owned dirs are still removed.
#
# This test drives the Step-7 emptiness logic in isolation (the exact `find`
# predicate the script uses) so it pins the `-type f` -> `-mindepth 1`
# regression without invoking the full uninstall.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"

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

# Extract the exact emptiness predicate the script uses so the test tracks the
# source of truth. This greps the `find ... -print -quit` line from Step 7 and
# asserts it is the `-mindepth 1` (fixed) form, not the `-type f` (buggy) form.
STEP7_FIND_LINE="$(grep -E 'remaining=\$\(find "\$dir_path"' "$UNINSTALL_SH" || true)"

echo ""
echo "=== Step 7 emptiness predicate is symlink-aware (source check) ==="

assert_eq "Step 7 uses -mindepth 1 (counts symlinks/subdirs)" \
  "yes" \
  "$(printf '%s' "$STEP7_FIND_LINE" | grep -q -- '-mindepth 1' && echo yes || echo no)"
assert_eq "Step 7 no longer uses the -type f predicate" \
  "yes" \
  "$(printf '%s' "$STEP7_FIND_LINE" | grep -q -- '-type f' && echo no || echo yes)"

# ---------------------------------------------------------------------------
# Behavioral test: replicate Step 7's emptiness check against a scaffolded tree
# where a Loom-owned dir contains ONLY a foreign co-installed symlink subdir.
# ---------------------------------------------------------------------------

# is_empty_by_step7 <dir> — mirrors the fixed Step 7 predicate. Prints "empty"
# or "nonempty". Kept as a small helper so the test asserts the same semantics
# the script applies.
is_empty_by_step7() {
  local dir_path="$1"
  local remaining
  remaining=$(find "$dir_path" -mindepth 1 -not -name '.DS_Store' -print -quit 2>/dev/null)
  if [[ -z "$remaining" ]]; then
    echo "empty"
  else
    echo "nonempty"
  fi
}

echo ""
echo "=== foreign symlink dir under .claude/commands/ survives cleanup ==="

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Foreign tool (e.g. Repo Skills) install target that the symlinks point at.
mkdir -p "$WORK/foreign-src/repo"
printf '# repo command\n' > "$WORK/foreign-src/repo/foo.md"
printf '# repo skill\n'   > "$WORK/foreign-src/repo/SKILL.md"

# Scaffold the target .claude tree AFTER Loom's own files were removed:
#   .claude/commands/          -> contains only the foreign symlink subdir repo/
#   .claude/commands/repo/     -> a dir of SYMLINKS owned by the sibling tool
#   .claude/skills/repo/       -> sibling symlink (asymmetry from the issue)
#   .claude/commands/loom/     -> Loom-owned, now genuinely empty
mkdir -p "$WORK/target/.claude/commands/repo"
mkdir -p "$WORK/target/.claude/commands/loom"
mkdir -p "$WORK/target/.claude/skills/repo"
ln -s "$WORK/foreign-src/repo/foo.md"   "$WORK/target/.claude/commands/repo/foo.md"
ln -s "$WORK/foreign-src/repo/SKILL.md" "$WORK/target/.claude/skills/repo/SKILL.md"

# Sanity: the foreign children are symlinks, not regular files. This is the crux
# of the regression — `find -type f` would miss them.
assert_eq "foreign commands/repo/foo.md is a symlink" \
  "yes" \
  "$([[ -L "$WORK/target/.claude/commands/repo/foo.md" ]] && echo yes || echo no)"

# The BUGGY predicate (-type f) would have deemed .claude/commands "empty".
BUGGY_REMAINING=$(find "$WORK/target/.claude/commands" -type f -not -name '.DS_Store' -print -quit 2>/dev/null)
assert_eq "buggy -type f predicate wrongly sees commands/ as empty (pins regression)" \
  "empty" \
  "$([[ -z "$BUGGY_REMAINING" ]] && echo empty || echo nonempty)"

# The FIXED predicate must see .claude/commands as NON-empty (holds repo/ subdir).
assert_eq ".claude/commands with only foreign symlink subdir is NOT empty" \
  "nonempty" \
  "$(is_empty_by_step7 "$WORK/target/.claude/commands")"

# Emulate Step 7 over the REMOVE_DIRS entries relevant here, innermost first.
for dir in ".claude/commands/loom" ".claude/commands" ".claude"; do
  dir_path="$WORK/target/$dir"
  if [[ -d "$dir_path" ]]; then
    if [[ "$(is_empty_by_step7 "$dir_path")" == "empty" ]]; then
      rm -rf "$dir_path"
    fi
  fi
done

# Assertions: foreign content survives byte-for-byte / link-for-link.
assert_eq "foreign .claude/commands/repo/ directory survives" \
  "yes" \
  "$([[ -d "$WORK/target/.claude/commands/repo" ]] && echo yes || echo no)"
assert_eq "foreign commands/repo/foo.md symlink survives and resolves" \
  "yes" \
  "$([[ -L "$WORK/target/.claude/commands/repo/foo.md" && -e "$WORK/target/.claude/commands/repo/foo.md" ]] && echo yes || echo no)"
assert_eq "foreign .claude/skills/repo/SKILL.md symlink survives" \
  "yes" \
  "$([[ -L "$WORK/target/.claude/skills/repo/SKILL.md" && -e "$WORK/target/.claude/skills/repo/SKILL.md" ]] && echo yes || echo no)"
assert_eq "parent .claude/commands survives (holds foreign subdir)" \
  "yes" \
  "$([[ -d "$WORK/target/.claude/commands" ]] && echo yes || echo no)"

# Genuinely-empty Loom-owned dir must still be removed (no regression).
assert_eq "empty Loom-owned .claude/commands/loom is still removed" \
  "yes" \
  "$([[ ! -d "$WORK/target/.claude/commands/loom" ]] && echo yes || echo no)"

# ---------------------------------------------------------------------------
# Control: a genuinely-empty Loom dir with nothing foreign is removed.
# ---------------------------------------------------------------------------
echo ""
echo "=== genuinely empty Loom dir is removed ==="

mkdir -p "$WORK/target2/.claude/agents"
assert_eq "empty .claude/agents deemed empty" \
  "empty" \
  "$(is_empty_by_step7 "$WORK/target2/.claude/agents")"

# A dir containing only .DS_Store is still deemed empty.
mkdir -p "$WORK/target2/.claude/commands"
printf '' > "$WORK/target2/.claude/commands/.DS_Store"
assert_eq "dir with only .DS_Store deemed empty" \
  "empty" \
  "$(is_empty_by_step7 "$WORK/target2/.claude/commands")"

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
