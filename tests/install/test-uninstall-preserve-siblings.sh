#!/usr/bin/env bash
# Test suite for uninstall-loom.sh Step 7 "Clean Up Empty Directories" (#3634).
#
# Usage: ./tests/install/test-uninstall-preserve-siblings.sh
#
# Two failure modes are pinned here — they pull the emptiness predicate in
# opposite directions, so the test asserts the predicate threads the needle:
#
#   1. Symlink-clobber (original bug): the predicate used `find "$dir" -type f`,
#      which does NOT count symlinks (type `l`). A co-installed tool (e.g. Repo
#      Skills) installs `.claude/commands/repo/` as a directory of *symlinks*.
#      After Loom removed its own files, `.claude/commands/` contained only that
#      foreign symlink subdir; `find -type f` reported it "empty" and `rm -rf`
#      clobbered a DIFFERENT tool's install. => foreign content must be PRESERVED.
#
#   2. Empty-subdir over-correction (the `-mindepth 1` regression): `-mindepth 1`
#      counts empty *subdirectories* as content. `REMOVE_DIRS` is NOT strictly
#      child-first (`.loom/scripts` precedes `.loom/scripts/cli`), so a parent
#      dir is checked while its still-empty child exists -> reported non-empty ->
#      left behind, orphaning real Loom cruft (breaks CI Test 29). => a Loom dir
#      holding only empty subdirs must still be REMOVED.
#
# The fix: `\( -type f -o -type l \) -not -name '.DS_Store'` — count files AND
# symlinks (foreign content to preserve) while ignoring empty dirs (Loom cruft to
# remove). Order-independent, so it satisfies both cases regardless of the
# non-child-first REMOVE_DIRS ordering.
#
# This test drives the Step-7 emptiness logic in isolation (the exact `find`
# predicate the script uses) and also replays the real REMOVE_DIRS ordering
# parsed from the script, so it pins both regressions without invoking the full
# uninstall.
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
# asserts it is the files-and-symlinks (fixed) form, not the `-type f`-only
# (symlink-clobber) or `-mindepth 1` (empty-subdir over-correction) forms.
STEP7_FIND_LINE="$(grep -E 'remaining=\$\(find "\$dir_path"' "$UNINSTALL_SH" || true)"

echo ""
echo "=== Step 7 emptiness predicate counts files AND symlinks (source check) ==="

assert_eq "Step 7 counts symlinks (-type l)" \
  "yes" \
  "$(printf '%s' "$STEP7_FIND_LINE" | grep -q -- '-type l' && echo yes || echo no)"
assert_eq "Step 7 counts files (-type f)" \
  "yes" \
  "$(printf '%s' "$STEP7_FIND_LINE" | grep -q -- '-type f' && echo yes || echo no)"
assert_eq "Step 7 no longer uses the over-correcting -mindepth 1 predicate" \
  "yes" \
  "$(printf '%s' "$STEP7_FIND_LINE" | grep -q -- '-mindepth 1' && echo no || echo yes)"

# ---------------------------------------------------------------------------
# is_empty_by_step7 <dir> — mirrors the fixed Step 7 predicate. Prints "empty"
# or "nonempty". Kept as a small helper so the test asserts the same semantics
# the script applies: files/symlinks = content, empty subdirs = cruft.
# ---------------------------------------------------------------------------
is_empty_by_step7() {
  local dir_path="$1"
  local remaining
  remaining=$(find "$dir_path" \( -type f -o -type l \) -not -name '.DS_Store' -print -quit 2>/dev/null)
  if [[ -z "$remaining" ]]; then
    echo "empty"
  else
    echo "nonempty"
  fi
}

# ---------------------------------------------------------------------------
# Case 1: foreign symlink dir under .claude/commands/ survives cleanup.
# ---------------------------------------------------------------------------

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

# The FIXED predicate must see .claude/commands as NON-empty (holds repo/ subdir
# of symlinks).
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
# Case 2 (empty-subdir over-correction): a Loom dir whose ONLY content is an
# empty Loom subdir must still be removed — even when the parent precedes the
# child in REMOVE_DIRS (the `.loom/scripts` before `.loom/scripts/cli` ordering
# that the `-mindepth 1` predicate regressed on / CI Test 29).
#
# This replays the REAL REMOVE_DIRS ordering parsed from the script, so the test
# fails if the array is ever reordered into a shape the predicate can't survive.
# ---------------------------------------------------------------------------
echo ""
echo "=== Loom dir holding only an empty Loom subdir is still removed (real REMOVE_DIRS order) ==="

# Parse REMOVE_DIRS from the script: the array literal spans `REMOVE_DIRS=(` to
# the closing `)`. Extract the quoted entries in source order. Uses a portable
# read loop (not `mapfile`, which is absent on stock macOS bash 3.2).
REMOVE_DIRS=()
while IFS= read -r entry; do
  REMOVE_DIRS+=("$entry")
done < <(
  awk '
    /^REMOVE_DIRS=\(/ { collecting = 1; next }
    collecting && /^\)/ { collecting = 0 }
    collecting {
      if (match($0, /"[^"]+"/)) {
        s = substr($0, RSTART + 1, RLENGTH - 2)
        print s
      }
    }
  ' "$UNINSTALL_SH"
)

assert_eq "REMOVE_DIRS parsed from script is non-empty" \
  "yes" \
  "$([[ ${#REMOVE_DIRS[@]} -gt 0 ]] && echo yes || echo no)"
# Confirm the non-child-first ordering the regression hinged on is present:
# .loom/scripts must appear BEFORE .loom/scripts/cli.
PARENT_IDX=-1
CHILD_IDX=-1
for i in "${!REMOVE_DIRS[@]}"; do
  [[ "${REMOVE_DIRS[$i]}" == ".loom/scripts" ]]     && PARENT_IDX=$i
  [[ "${REMOVE_DIRS[$i]}" == ".loom/scripts/cli" ]] && CHILD_IDX=$i
done
assert_eq ".loom/scripts precedes .loom/scripts/cli in REMOVE_DIRS (non-child-first)" \
  "yes" \
  "$([[ $PARENT_IDX -ge 0 && $CHILD_IDX -ge 0 && $PARENT_IDX -lt $CHILD_IDX ]] && echo yes || echo no)"

# Scaffold a target where the ONLY Loom content left after file removal is the
# empty-dir tree .loom/scripts + .loom/scripts/cli (both empty). Also drop an
# empty .loom/roles to be sure genuinely-empty siblings go too.
mkdir -p "$WORK/target3/.loom/scripts/cli"
mkdir -p "$WORK/target3/.loom/roles"

# Replay Step 7 exactly: iterate REMOVE_DIRS in source order, applying the fixed
# predicate. Because the parent (.loom/scripts) precedes its empty child
# (.loom/scripts/cli), a subdir-counting predicate would wrongly keep the parent.
for dir in "${REMOVE_DIRS[@]}"; do
  dir_path="$WORK/target3/$dir"
  if [[ -d "$dir_path" ]]; then
    if [[ "$(is_empty_by_step7 "$dir_path")" == "empty" ]]; then
      rm -rf "$dir_path"
    fi
  fi
done

assert_eq ".loom/scripts/cli (empty child) removed" \
  "yes" \
  "$([[ ! -d "$WORK/target3/.loom/scripts/cli" ]] && echo yes || echo no)"
assert_eq ".loom/scripts (parent holding only empty child) removed — NOT orphaned" \
  "yes" \
  "$([[ ! -d "$WORK/target3/.loom/scripts" ]] && echo yes || echo no)"
assert_eq ".loom/roles (empty sibling) removed" \
  "yes" \
  "$([[ ! -d "$WORK/target3/.loom/roles" ]] && echo yes || echo no)"
assert_eq ".loom (holding only empty subdir tree) removed" \
  "yes" \
  "$([[ ! -d "$WORK/target3/.loom" ]] && echo yes || echo no)"

# Cross-check: the OVER-CORRECTING `-mindepth 1` predicate would have KEPT
# .loom/scripts here (its empty cli/ child counts as content). Pin that so a
# regression back to -mindepth 1 is caught even if is_empty_by_step7 drifts.
mkdir -p "$WORK/target3-mindepth/.loom/scripts/cli"
MINDEPTH_REMAINING=$(find "$WORK/target3-mindepth/.loom/scripts" -mindepth 1 -not -name '.DS_Store' -print -quit 2>/dev/null)
assert_eq "over-correcting -mindepth 1 wrongly sees .loom/scripts as non-empty (pins regression)" \
  "nonempty" \
  "$([[ -z "$MINDEPTH_REMAINING" ]] && echo empty || echo nonempty)"

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
