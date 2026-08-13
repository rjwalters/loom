#!/usr/bin/env bash
# Test suite for uninstall-loom.sh's --clean directory sweep and .loom/hooks/
# ownership boundary (#5971).
#
# Usage: ./tests/install/test-uninstall-clean-preserves-unmanaged-hooks.sh
#
# Background: `.loom/hooks/` is a documented extension point Loom itself
# invokes (worktree.sh's post-worktree.sh hook) -- a repo is expected to add
# its own files there, and `.loom/roles/` likewise accepts consumer-authored
# custom roles. Epic #3835 Phase 5 (#4262) also deliberately excludes hooks/*
# from the installed-files manifest, so the manifest can never confirm a hooks
# file is Loom-owned. Before this fix, uninstall-loom.sh's --clean sweep walked
# .loom/roles|scripts|docs|hooks and queued EVERY file found there for hard
# deletion, with no ownership check at all -- silently deleting a repo-owned
# hook (the reported incident: kicad-tools' repo-owned
# .loom/hooks/post-worktree.sh was deleted on a --confirm-reinstall upgrade).
#
# The rule this suite pins, for uninstall-loom.sh's operator-requested
# --clean sweep:
#   * a path declared in `.loom/resync-ignore` is never removed, anywhere;
#   * inside `.loom/hooks/` specifically, a file the current defaults/ does not
#     ship is preserved even without a declaration (it is an invoked extension
#     point whose contents can never appear in the manifest);
#   * elsewhere, --clean keeps its documented "including unknown files"
#     contract -- the declaration is the opt-out.
# The Rust reinstall sweep (loom-daemon init) is strictly more conservative;
# its half of the boundary is unit-tested in loom-daemon/src/init/.
#
# This suite runs the REAL uninstall-loom.sh end-to-end (--yes --local
# --clean, no network/gh dependency) against scaffolded target repos and
# asserts:
#   1. A stale but genuinely Loom-shipped hook copy is still removed (no
#      regression -- --clean must still be able to clean these up).
#   2. An unmanaged (repo-owned) hook file survives, byte-for-byte, and is
#      named in the script's output rather than silently removed.
#   3. A file that collides by name with a real Loom-shipped hook, but is
#      declared repo-owned via `.loom/resync-ignore`, survives even though
#      the basename matches.
#   4. The plain (non---clean) --local path -- the one the reported repro
#      actually takes -- leaves the unmanaged hook alone.
#   5. Outside hooks/, --clean still removes an unpinned custom role (its
#      documented contract) but honors a .loom/resync-ignore pin.
#   6. A declared repo-owned path survives even when the previous install's
#      `installed_files` manifest wrongly claims Loom wrote it.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

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

# make_target <dir> — scaffold a minimal, valid uninstall target: a git repo
# with a .loom/ tree and an install-metadata.json carrying an (empty)
# installed_files manifest, so uninstall-loom.sh takes the manifest-based
# removal path (the one the --clean sweep lives in).
make_target() {
  local target="$1"
  mkdir -p "$target/.loom/hooks" "$target/.loom/roles" "$target/.loom/scripts" "$target/.loom/docs"
  git -C "$target" init -q
  git -C "$target" config user.email "test@example.com"
  git -C "$target" config user.name "test"
  printf '{"installed_files": []}\n' > "$target/.loom/install-metadata.json"
}

# ============================================================================
# Test 1: a genuinely Loom-shipped hook basename is still cleaned up by
# --clean (no regression -- this is the sweep's actual intended purpose,
# per scripts/install/manifest.sh's hooks/* CAVEAT comment: install.sh
# --quick copies defaults/hooks/*.sh into .loom/hooks/ but deliberately
# leaves them out of the manifest, so --clean is the only path that retires
# a stale copy).
# ============================================================================
echo ""
echo "=== --clean still removes a stale, genuinely Loom-shipped hook copy ==="

TARGET1="$WORK/target1"
make_target "$TARGET1"
cp "$REPO_ROOT/defaults/hooks/guard-destructive.sh" "$TARGET1/.loom/hooks/guard-destructive.sh"
git -C "$TARGET1" add -A
git -C "$TARGET1" commit -q -m init

OUT1="$("$UNINSTALL_SH" --yes --local --clean "$TARGET1" 2>&1)"

assert_eq "stale Loom-shipped hooks/guard-destructive.sh is removed" \
  "no" \
  "$([[ -f "$TARGET1/.loom/hooks/guard-destructive.sh" ]] && echo yes || echo no)"
assert_eq "removal output names the removed hook file" \
  "yes" \
  "$(printf '%s' "$OUT1" | grep -qF '.loom/hooks/guard-destructive.sh' && echo yes || echo no)"

# ============================================================================
# Test 2 (the reported incident, #5971): an unmanaged repo-owned hook file
# (basename never shipped by defaults/hooks/) survives --clean byte-for-byte,
# and is explicitly named as preserved rather than silently dropped.
# ============================================================================
echo ""
echo "=== --clean preserves an unmanaged repo-owned .loom/hooks/ file ==="

TARGET2="$WORK/target2"
make_target "$TARGET2"
MARKER='# REPO-OWNED kicad-tools-style project hook (issue #4558 precedent)'
cat > "$TARGET2/.loom/hooks/post-worktree.sh" <<EOF
#!/bin/bash
$MARKER
cd "\$1" && uv sync --frozen --extra dev
EOF
git -C "$TARGET2" add -A
git -C "$TARGET2" commit -q -m init

OUT2="$("$UNINSTALL_SH" --yes --local --clean "$TARGET2" 2>&1)"

assert_eq "unmanaged hooks/post-worktree.sh survives --clean" \
  "yes" \
  "$([[ -f "$TARGET2/.loom/hooks/post-worktree.sh" ]] && echo yes || echo no)"
assert_eq "survived file content is byte-for-byte unchanged" \
  "yes" \
  "$(grep -qF "$MARKER" "$TARGET2/.loom/hooks/post-worktree.sh" 2>/dev/null && echo yes || echo no)"
assert_eq "preservation is named in the script's output, not silent" \
  "yes" \
  "$(printf '%s' "$OUT2" | grep -qF 'preserving .loom/hooks/post-worktree.sh' && echo yes || echo no)"

# ============================================================================
# Test 3: a file whose basename collides with a real Loom-shipped hook, but
# is declared repo-owned via .loom/resync-ignore, survives despite the
# collision (the edge case the Curator flagged: a customized fork of a
# Loom-named hook must not be force-deleted by --clean).
# ============================================================================
echo ""
echo "=== --clean honors .loom/resync-ignore even when the basename collides ==="

TARGET3="$WORK/target3"
make_target "$TARGET3"
FORK_MARKER='# CUSTOMIZED FORK -- do not overwrite/delete'
printf '#!/bin/bash\n%s\n' "$FORK_MARKER" > "$TARGET3/.loom/hooks/guard-loom-workflow.sh"
printf 'hooks/guard-loom-workflow.sh\n' > "$TARGET3/.loom/resync-ignore"
git -C "$TARGET3" add -A
git -C "$TARGET3" commit -q -m init

OUT3="$("$UNINSTALL_SH" --yes --local --clean "$TARGET3" 2>&1)"

assert_eq "resync-ignore-pinned hooks/guard-loom-workflow.sh survives --clean" \
  "yes" \
  "$([[ -f "$TARGET3/.loom/hooks/guard-loom-workflow.sh" ]] && echo yes || echo no)"
assert_eq "pinned file content is unchanged" \
  "yes" \
  "$(grep -qF "$FORK_MARKER" "$TARGET3/.loom/hooks/guard-loom-workflow.sh" 2>/dev/null && echo yes || echo no)"
assert_eq "resync-ignore preservation is named in the script's output" \
  "yes" \
  "$(printf '%s' "$OUT3" | grep -qF 'preserving .loom/hooks/guard-loom-workflow.sh (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

# ============================================================================
# Test 4: plain (non --clean) reinstall path also preserves the unmanaged
# hook -- covers the literal reported repro
# (`install.sh --quick --yes --confirm-reinstall`, which chains a non---clean
# `uninstall-loom.sh --yes --local`).
# ============================================================================
echo ""
echo "=== plain (non --clean) --local uninstall never touches .loom/hooks/ unmanaged files ==="

TARGET4="$WORK/target4"
make_target "$TARGET4"
printf '#!/bin/bash\n%s\n' "$MARKER" > "$TARGET4/.loom/hooks/post-worktree.sh"
git -C "$TARGET4" add -A
git -C "$TARGET4" commit -q -m init

"$UNINSTALL_SH" --yes --local "$TARGET4" >/dev/null 2>&1

assert_eq "unmanaged hooks/post-worktree.sh survives a plain --local uninstall" \
  "yes" \
  "$([[ -f "$TARGET4/.loom/hooks/post-worktree.sh" ]] && echo yes || echo no)"

# ============================================================================
# Test 5: outside .loom/hooks/, --clean keeps its documented contract ("remove
# all files in managed directories, including unknown files") -- an operator
# who passes --clean asked for that. The declaration is the opt-out: a custom
# role pinned in .loom/resync-ignore survives, an unpinned one does not. This
# pins BOTH halves so neither can drift.
# ============================================================================
echo ""
echo "=== outside hooks/, --clean removes an unpinned custom role but honors a pin ==="

TARGET5="$WORK/target5"
make_target "$TARGET5"
ROLE_MARKER='# Custom project role, authored by this repo'
printf '%s\n' "$ROLE_MARKER" > "$TARGET5/.loom/roles/designer.md"
printf '%s\n' "$ROLE_MARKER" > "$TARGET5/.loom/roles/pinned-role.md"
printf 'roles/pinned-role.md\n' > "$TARGET5/.loom/resync-ignore"
git -C "$TARGET5" add -A
git -C "$TARGET5" commit -q -m init

OUT5="$("$UNINSTALL_SH" --yes --local --clean "$TARGET5" 2>&1)"

assert_eq "unpinned roles/designer.md is still removed by --clean (contract preserved)" \
  "no" \
  "$([[ -f "$TARGET5/.loom/roles/designer.md" ]] && echo yes || echo no)"
assert_eq "pinned roles/pinned-role.md survives --clean" \
  "yes" \
  "$([[ -f "$TARGET5/.loom/roles/pinned-role.md" ]] && echo yes || echo no)"
assert_eq "pinned custom role content is unchanged" \
  "yes" \
  "$(grep -qF "$ROLE_MARKER" "$TARGET5/.loom/roles/pinned-role.md" 2>/dev/null && echo yes || echo no)"
assert_eq "pinned-role preservation is named in the output" \
  "yes" \
  "$(printf '%s' "$OUT5" | grep -qF 'preserving .loom/roles/pinned-role.md (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

# ============================================================================
# Test 6: a declared repo-owned path outranks the manifest. A legacy
# over-broad `installed_files` entry must not be able to delete a file the
# repo explicitly claimed in .loom/resync-ignore.
# ============================================================================
echo ""
echo "=== .loom/resync-ignore outranks an installed_files manifest entry ==="

TARGET6="$WORK/target6"
make_target "$TARGET6"
CLAIMED_MARKER='# repo-owned despite the stale manifest entry'
printf '%s\n' "$CLAIMED_MARKER" > "$TARGET6/.loom/scripts/project-local.sh"
printf 'scripts/project-local.sh\n' > "$TARGET6/.loom/resync-ignore"
printf '{"installed_files": [".loom/scripts/project-local.sh"]}\n' \
  > "$TARGET6/.loom/install-metadata.json"
git -C "$TARGET6" add -A
git -C "$TARGET6" commit -q -m init

OUT6="$("$UNINSTALL_SH" --yes --local "$TARGET6" 2>&1)"

assert_eq "manifest-listed but declared repo-owned file survives" \
  "yes" \
  "$([[ -f "$TARGET6/.loom/scripts/project-local.sh" ]] && echo yes || echo no)"
assert_eq "declared-ownership preservation is named in the output" \
  "yes" \
  "$(printf '%s' "$OUT6" | grep -qF 'preserving .loom/scripts/project-local.sh (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

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
