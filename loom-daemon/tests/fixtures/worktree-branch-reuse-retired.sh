#!/usr/bin/env bash
# worktree-branch-reuse-retired.sh — FROZEN copy of the shell #8195 slice 14
# retired.
#
# `defaults/scripts/worktree.sh`'s LOCAL-branch reuse arm — the body of
# `if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then` — exactly
# as it stood immediately before the port to `loom-daemon
# worktree-branch-reuse`. Sole consumer:
# `loom-daemon/tests/worktree_branch_reuse_differential.rs`.
#
# DO NOT "FIX" ANYTHING HERE. The hand-spliced `--json` refusal document is a
# COMPARED DEFECT: it is not valid JSON for any `$BRANCH_NAME` containing a
# quote, and the differential asserts that the port's document is.
#
# Usage: worktree-branch-reuse-retired.sh <branch> <issue> <default-branch> \
#                                         <base-ref> <base-display> <json:true|false>
# Runs in the current directory (the main workspace). The `--json` document
# goes to stdout, which is the live script's fd 3.
set -uo pipefail

LIB="${LOOM_BRANCH_REUSE_RETIRED_LIB:?path to defaults/scripts/lib/branch-landed.sh}"
# shellcheck source=/dev/null
source "$LIB"

BRANCH_NAME="$1"; ISSUE_NUMBER="$2"; DEFAULT_BRANCH="$3"
BASE_REF="$4"; BASE_DISPLAY="$5"; JSON_OUTPUT="$6"

print_error()   { echo "ERROR: $1" >&2; }
print_warning() { echo "⚠ $1"; }
exec 3>&1

# The slice-9 `_worktree_upstream_check` delegation, verbatim. Copied rather
# than sourced, for the reason worktree-existing-retired.sh gives at its own
# copy: the wrapper is what this slice retires, so it must be frozen here.
_WT_DAEMON_BIN="${LOOM_DAEMON_SELF_BIN:-}"
_worktree_upstream_check() {
    [[ -n "${_WT_DAEMON_BIN:-}" ]] || return 0

    local _q="" _u=""
    [[ "$JSON_OUTPUT" == "true" ]] && _q="--quiet"
    [[ -n "${3:-}" ]] && _u="--uncommitted"
    # shellcheck disable=SC2086
    "$_WT_DAEMON_BIN" worktree-upstream --arm "$1" --repo "$2" \
        --branch "$BRANCH_NAME" --issue "$ISSUE_NUMBER" $_q $_u || true
    return 0
}

# --- the retired arm, verbatim from here down -------------------------------

if [[ "$JSON_OUTPUT" != "true" ]]; then
    print_warning "Branch '$BRANCH_NAME' already exists - reusing it (for a fresh branch instead, pass a custom name: ./.loom/scripts/worktree.sh $ISSUE_NUMBER <custom-branch-name>)"
fi

_worktree_upstream_check local-branch "$PWD"

branch_landed "$BRANCH_NAME" "$DEFAULT_BRANCH"
if [[ "$BRANCH_LANDED_VERDICT" == "landed" ]] && [[ "$(git rev-parse "$BRANCH_NAME" 2>/dev/null)" != "$(git rev-parse "origin/$DEFAULT_BRANCH" 2>/dev/null)" ]]; then
    if [[ "$JSON_OUTPUT" == "true" ]]; then echo '{"success": false, "error": "branch-already-landed", "issueNumber": '"$ISSUE_NUMBER"', "branch": "'"$BRANCH_NAME"'", "prNumber": '"${BRANCH_LANDED_PR_NUMBER:-null}"'}' >&3; else print_error "Local branch '$BRANCH_NAME' has already landed on $BASE_DISPLAY${BRANCH_LANDED_PR_NUMBER:+ (already-merged PR #$BRANCH_LANDED_PR_NUMBER)} - refusing to reuse it. Delete it and re-run: git branch -D $BRANCH_NAME && ./.loom/scripts/worktree.sh $ISSUE_NUMBER"; fi
    exit 1
fi
if [[ "$JSON_OUTPUT" != "true" ]] && ! git merge-base --is-ancestor "$BASE_REF" "$BRANCH_NAME" 2>/dev/null; then print_warning "Branch '$BRANCH_NAME' has diverged from $BASE_DISPLAY (does not contain all of its history) - reusing it as-is; rebase or delete it if that is not what you want"; fi

exit 0
