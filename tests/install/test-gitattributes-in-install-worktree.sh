#!/usr/bin/env bash
# shellcheck disable=SC2016  # single-quoted bash -c bodies are intentional
# Regression test for #12683: install-loom.sh must write the merge-driver
# .gitattributes into the install worktree (so it lands in the install PR),
# not the target's main checkout (where it stays untracked after the merge).
#
# Usage: ./tests/install/test-gitattributes-in-install-worktree.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

FAIL=0
check() {
  local desc="$1"; shift
  if "$@"; then echo "PASS: $desc"; else echo "FAIL: $desc"; FAIL=1; fi
}

# 1. The install-loom.sh call site targets the worktree, never bare TARGET_PATH.
check "install-loom.sh passes the install worktree to the merge-driver helper" \
  grep -qE '^ensure_install_metadata_merge_driver "\$TARGET_PATH/\$WORKTREE_PATH"$' \
  "$REPO_ROOT/scripts/install-loom.sh"
check "install-loom.sh no longer passes the main checkout" \
  bash -c '! grep -qE "^ensure_install_metadata_merge_driver \"\$TARGET_PATH\"$" "$1"' _ \
  "$REPO_ROOT/scripts/install-loom.sh"

# 2. Behavior: helper run against a linked worktree leaves the main checkout
#    clean and puts a stageable .gitattributes in the worktree.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
git init -q "$TMP/main"
git -C "$TMP/main" -c user.name=t -c user.email=t@t commit -q --allow-empty -m init
git -C "$TMP/main" worktree add -q "$TMP/wt" -b install-branch

# shellcheck source=/dev/null
source "$REPO_ROOT/scripts/install/gitattributes.sh"
ensure_install_metadata_merge_driver "$TMP/wt"

check "worktree has .gitattributes with the merge rule" \
  grep -qF ".loom/install-metadata.json merge=ours" "$TMP/wt/.gitattributes"
check "main checkout has no .gitattributes" test ! -e "$TMP/main/.gitattributes"
check "main checkout git status is clean" \
  test -z "$(git -C "$TMP/main" status --porcelain)"
git -C "$TMP/wt" add -A
check ".gitattributes is staged in the worktree (lands in the PR)" \
  bash -c 'git -C "$1" diff --cached --name-only | grep -qx .gitattributes' _ "$TMP/wt"

exit "$FAIL"
