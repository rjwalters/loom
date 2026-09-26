#!/usr/bin/env bash
# worktree-cleanup-retired.sh — FROZEN copy of the shell this slice retired.
#
# This is `cleanup_partial_worktree_state()` exactly as it stood in
# `defaults/scripts/worktree.sh` immediately before #8195 slice 5 replaced it
# with a delegation to `loom-daemon worktree-cleanup`. It exists for exactly
# one consumer: `loom-daemon/tests/worktree_cleanup_differential.rs`.
#
# WHY A FROZEN COPY RATHER THAN THE LIVE SOURCE
#
# The retained suite (`test-worktree-orphan-guard-spaces.sh`) scraped the
# function body out of the live `worktree.sh` and eval'd it — deliberately, so
# a hand-copied `awk` program could not drift from the implementation it
# claimed to cover. That stopped being possible the moment `worktree.sh`
# started delegating: there is no body left to scrape. Reading it out of git
# history instead would pin the test to a moving ref.
#
# DO NOT "FIX" ANYTHING HERE. A bug in this file is the point — it is the
# behaviour the port is being compared against, including the parts that are
# only correct because #7849 corrected them (`substr($0, 10)` and `pwd -P`).
#
# WHAT IS STUBBED, AND WHY THAT IS NOT A CHEAT
#
#   print_warning      — the real one is four lines of ANSI in worktree.sh.
#                        The differential compares warning TEXT, not colour.
#   loom_worktree_root — stubbed to its documented DEFAULT tier,
#                        `<repo_root>/.loom/worktrees`, the same way
#                        `test-worktree-orphan-guard-spaces.sh` stubs it. The
#                        corpus configures no override on either side, and the
#                        Rust side is run with the private-defaults tier
#                        disabled, so both reach the same default. Sourcing
#                        `lib/worktree-root.sh` instead would make this file
#                        depend on a live, unfrozen one.
#
# Usage: worktree-cleanup-retired.sh <issue>     (run from the intended cwd)

set -uo pipefail

JSON_OUTPUT="${JSON_OUTPUT:-false}"

print_warning() { echo "⚠ $1"; }

loom_worktree_root() { echo "$1/.loom/worktrees"; }

# ===== BEGIN FROZEN BODY =====================================================
cleanup_partial_worktree_state() {
    local issue="$1"
    local git_common
    git_common=$(git rev-parse --git-common-dir 2>/dev/null) || return 0

    local admin_dir="$git_common/worktrees/issue-$issue"
    local cleaned=0

    # 1. Per-worktree file locks.
    local lf
    for lf in index.lock HEAD.lock gitdir.lock; do
        if [[ -f "$admin_dir/$lf" ]]; then
            rm -f "$admin_dir/$lf" 2>/dev/null && cleaned=1
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Cleaned stale $lf at $admin_dir/$lf"
            fi
        fi
    done

    # 2. Orphan worktree dir (exists but git doesn't know about it).
    local repo_root
    repo_root=$(cd "$(dirname "$git_common")" 2>/dev/null && pwd) || repo_root="$(pwd)"
    local wt_path
    wt_path="$(loom_worktree_root "$repo_root")/issue-$issue"
    if [[ -d "$wt_path" ]]; then
        local abs_wt
        abs_wt=$(cd "$wt_path" 2>/dev/null && pwd -P) || abs_wt=""
        local registered=0
        if [[ -n "$abs_wt" ]]; then
            if git worktree list --porcelain 2>/dev/null \
                | awk '/^worktree / {print substr($0, 10)}' \
                | grep -Fxq "$abs_wt"; then
                registered=1
            fi
        fi
        if [[ $registered -eq 0 ]]; then
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Removing orphan worktree dir (not registered with git): $wt_path"
            fi
            rm -rf "$wt_path" 2>/dev/null && cleaned=1
        fi
    fi

    # 3. Prune now that the orphan administrative dir is locally consistent.
    if [[ $cleaned -eq 1 ]]; then
        git worktree prune 2>/dev/null || true
    fi
}
# ===== END FROZEN BODY =======================================================

cleanup_partial_worktree_state "$1"
exit 0
