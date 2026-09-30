#!/usr/bin/env bash
# worktree-base-retired.sh — FROZEN copy of the shell #8195 slice 13 retired.
#
# `defaults/scripts/worktree.sh`'s `fetch_latest_main` and its `--base`
# stacked-PR resolution block exactly as they stood immediately before the port
# to `loom-daemon worktree-base`. Sole consumer:
# `loom-daemon/tests/worktree_base_differential.rs`. DO NOT "FIX" ANYTHING HERE:
# the hand-spliced JSON in the two `--base` refusal arms IS a compared defect.
#
# Usage: worktree-base-retired.sh <default-branch> <base-branch|""> <json:true|false>
# Runs in the current directory. Prints `BASE_REF=..` / `BASE_DISPLAY=..` last on
# success; the `--json` documents go to stdout (the live script's fd 3).
set -euo pipefail
LIB="${LOOM_BASE_RETIRED_LIB:?path to defaults/scripts/lib/default-branch.sh}"
# shellcheck source=/dev/null
source "$LIB"
DEFAULT_BRANCH="$1"; BASE_BRANCH="$2"; JSON_OUTPUT="$3"
print_error()   { echo "ERROR: $1" >&2; }
print_success() { echo "✓ $1"; }
print_info()    { echo "ℹ $1"; }
print_warning() { echo "⚠ $1"; }
exec 3>&1

fetch_latest_main() {
    local quiet=""
    [[ "$JSON_OUTPUT" == "true" ]] && quiet=1
    [[ -n "$quiet" ]] || print_info "Fetching latest changes from origin/$DEFAULT_BRANCH..."
    if git fetch origin -- "$DEFAULT_BRANCH" 2>/dev/null; then
        [[ -n "$quiet" ]] || print_success "Fetched latest origin/$DEFAULT_BRANCH"
    else
        [[ -n "$quiet" ]] || print_warning "Could not fetch origin/$DEFAULT_BRANCH (continuing with local state)"
    fi
}

fetch_latest_main

BASE_REF="origin/$DEFAULT_BRANCH"
BASE_DISPLAY="$DEFAULT_BRANCH"
if [[ -n "$BASE_BRANCH" ]]; then
    check_branch_name "$BASE_BRANCH" "--base branch" || {
        [[ "$JSON_OUTPUT" == "true" ]] && echo '{"success": false, "error": "unsafe-base-branch-name", "baseBranch": "'"$BASE_BRANCH"'"}' >&3
        exit 1
    }
    git fetch origin -- "$BASE_BRANCH" 2>/dev/null || true
    if git show-ref --verify --quiet "refs/remotes/origin/$BASE_BRANCH"; then
        BASE_REF="origin/$BASE_BRANCH"; BASE_DISPLAY="$BASE_REF"
    elif git show-ref --verify --quiet "refs/heads/$BASE_BRANCH"; then
        BASE_REF="$BASE_BRANCH"; BASE_DISPLAY="$BASE_REF"
    else
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            echo '{"success": false, "error": "base-branch-not-found", "baseBranch": "'"$BASE_BRANCH"'"}' >&3
        else
            print_error "Requested --base '$BASE_BRANCH' not found as origin/$BASE_BRANCH or a local branch."
            echo "  Ensure the parent sweep has created/pushed feature/issue-<parent> before stacking a child on it."
        fi
        exit 1
    fi
    [[ "$JSON_OUTPUT" == "true" ]] || print_info "Stacked worktree base: $BASE_DISPLAY (from --base $BASE_BRANCH)"
fi
echo "BASE_REF=$BASE_REF"
echo "BASE_DISPLAY=$BASE_DISPLAY"
