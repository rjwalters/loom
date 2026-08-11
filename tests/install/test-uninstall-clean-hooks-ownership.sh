#!/usr/bin/env bash
# Test suite for uninstall-loom.sh's CLEAN_MODE non-manifest sweep (issue #5971).
#
# Usage: ./tests/install/test-uninstall-clean-hooks-ownership.sh
#
# Background: `uninstall-loom.sh --clean` walks LOOM_OWNED_DIRS
# (.loom/roles, .loom/scripts, .loom/docs, .loom/hooks) and used to add EVERY
# file found there to the removal list, regardless of whether the current
# Loom defaults/ actually ships it. `.loom/hooks/` is genuinely
# mixed-ownership -- it is a documented extension point
# (defaults/scripts/worktree.sh's post-worktree.sh hook) that consumer repos
# are expected to write their own files into -- so the unconditional sweep
# silently deleted a real consumer repo's `.loom/hooks/post-worktree.sh` on a
# `--clean` reinstall (the incident reported in #5971).
#
# The fix intersects CLEAN_MODE sweep candidates under `.loom/hooks/` against
# the same LOOM_OWNERSHIP_SET (derived from the CURRENT defaults/) already
# used by the manifest-based removal loop, so a hook file the current
# defaults/ does not ship is preserved and reported instead of silently
# rm -f'd.
#
# The fix is deliberately scoped to `.loom/hooks/` -- the one directory #5971
# identifies as a mixed-ownership extension point. `.loom/roles`,
# `.loom/scripts`, and `.loom/docs` are Loom-exclusive and keep `--clean`'s
# original "wipe everything, managed or not" contract (asserted here, and by
# scripts/test-installer.sh Test 28).
#
# This suite runs the REAL uninstall-loom.sh end-to-end (--yes --local
# --clean) against scaffolded throwaway git repos, so it exercises the actual
# removal manifest, not a reimplementation of its logic.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"

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

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# scaffold_target <dir> — creates a minimal git repo Loom "installed" into,
# with an install-metadata.json whose installed_files is empty (mirroring the
# real incident: the consumer file is correctly absent from the manifest).
scaffold_target() {
  local dir="$1"
  mkdir -p "$dir/.loom/hooks" "$dir/.loom/roles"
  git -C "$dir" init -q
  git -C "$dir" config user.email "test@test.com"
  git -C "$dir" config user.name "Test"
  printf '{"installed_files": []}' > "$dir/.loom/install-metadata.json"
}

run_uninstall_clean() {
  local dir="$1"
  local log="$2"
  "$UNINSTALL_SH" --yes --local --clean "$dir" > "$log" 2>&1
}

echo ""
echo "=== Source anchors: CLEAN_MODE sweep intersects against LOOM_OWNERSHIP_SET ==="

assert_eq "CLEAN_MODE block references LOOM_OWNERSHIP_SET" "yes" \
  "$(awk '/CLEAN_MODE" == "true" \]\]; then/{f=1} f && /LOOM_OWNERSHIP_SET/{print "yes"; exit} /^  fi$/{if(f) exit}' "$UNINSTALL_SH")"
assert_eq "CLEAN_MODE preserves via PRESERVED_NOT_OWNED (not unconditional REMOVE_FILES)" "yes" \
  "$(grep -q 'PRESERVED_NOT_OWNED_CLEAN_SWEEP+=("\$rel_file")' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "ownership intersection is scoped to .loom/hooks only" "yes" \
  "$(grep -q 'if \[\[ "\$loom_dir" == ".loom/hooks" \]\]' "$UNINSTALL_SH" && echo yes || echo no)"

echo ""
echo "=== Repo-owned .loom/hooks/post-worktree.sh survives --clean reinstall (the #5971 incident) ==="

T1="$WORK/t1"
scaffold_target "$T1"
printf '#!/usr/bin/env bash\necho "repo-owned hook (issue #4558-style)"\n' > "$T1/.loom/hooks/post-worktree.sh"
chmod +x "$T1/.loom/hooks/post-worktree.sh"
git -C "$T1" add -A
git -C "$T1" commit -q -m "init"

LOG1="$WORK/t1.log"
run_uninstall_clean "$T1" "$LOG1"

assert_eq "post-worktree.sh survives --clean" "yes" \
  "$([[ -f "$T1/.loom/hooks/post-worktree.sh" ]] && echo yes || echo no)"
assert_eq "post-worktree.sh content is untouched" "yes" \
  "$(grep -q 'repo-owned hook' "$T1/.loom/hooks/post-worktree.sh" 2>/dev/null && echo yes || echo no)"
assert_eq "preservation is reported in output (reviewable, not silent)" "yes" \
  "$(grep -q 'preserving .loom/hooks/post-worktree.sh' "$LOG1" && echo yes || echo no)"

echo ""
echo "=== A same-named-as-Loom-shipped .loom/hooks/ file also survives (per-repo hook copies are outside the ownership boundary, #4262) ==="

REAL_HOOK_NAME="$(find "$REPO_ROOT/defaults/hooks" -maxdepth 1 -name '*.sh' | sort | head -1 | xargs -I{} basename {})"
T2="$WORK/t2"
scaffold_target "$T2"
printf '#!/usr/bin/env bash\necho "custom override, not the shipped content"\n' > "$T2/.loom/hooks/$REAL_HOOK_NAME"
chmod +x "$T2/.loom/hooks/$REAL_HOOK_NAME"
git -C "$T2" add -A
git -C "$T2" commit -q -m "init"

LOG2="$WORK/t2.log"
run_uninstall_clean "$T2" "$LOG2"

assert_eq "same-named hook file survives --clean" "yes" \
  "$([[ -f "$T2/.loom/hooks/$REAL_HOOK_NAME" ]] && echo yes || echo no)"
assert_eq "same-named hook file content is untouched (not silently reset to shipped content)" "yes" \
  "$(grep -q 'custom override' "$T2/.loom/hooks/$REAL_HOOK_NAME" 2>/dev/null && echo yes || echo no)"

echo ""
echo "=== Scope guard: an unmanaged file under a Loom-EXCLUSIVE dir (.loom/roles/) is STILL removed by --clean ==="
# The preserve semantics are scoped to .loom/hooks/ (the only mixed-ownership
# extension point #5971 identifies). .loom/roles/ keeps --clean's original
# "wipe everything under a Loom-owned dir" contract -- the same behavior
# scripts/test-installer.sh Test 28 asserts. If this assertion is ever
# deliberately inverted, Test 28 must be updated in the same change.

T3="$WORK/t3"
scaffold_target "$T3"
printf '# a consumer-authored role file Loom never shipped\n' > "$T3/.loom/roles/consumer-only-role.md"
git -C "$T3" add -A
git -C "$T3" commit -q -m "init"

LOG3="$WORK/t3.log"
run_uninstall_clean "$T3" "$LOG3"

assert_eq "consumer-only-role.md is removed by --clean (scope limited to .loom/hooks/)" "no" \
  "$([[ -f "$T3/.loom/roles/consumer-only-role.md" ]] && echo yes || echo no)"

echo ""
echo "=== No regression: a genuinely Loom-owned stale file under .loom/roles/ is STILL removed by --clean ==="

T4="$WORK/t4"
scaffold_target "$T4"
cp "$REPO_ROOT/defaults/roles/architect.md" "$T4/.loom/roles/architect.md"
git -C "$T4" add -A
git -C "$T4" commit -q -m "init"

LOG4="$WORK/t4.log"
run_uninstall_clean "$T4" "$LOG4"

assert_eq "architect.md (real Loom-owned role file, absent from stale manifest) is removed by --clean" "no" \
  "$([[ -f "$T4/.loom/roles/architect.md" ]] && echo yes || echo no)"

echo ""
echo "==================================================="
echo "Results: $PASS/$TOTAL passed, $FAIL failed"
echo "==================================================="
[[ "$FAIL" -eq 0 ]]
