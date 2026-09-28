#!/usr/bin/env bash
# worktree-upstream-retired.sh — FROZEN copy of the shell this slice retired.
#
# These are the two blocks of `defaults/scripts/worktree.sh` exactly as they
# stood immediately before #8195 slice 9 replaced them with a delegation to
# `loom-daemon worktree-upstream`:
#
#   local-branch         the "local branch already exists - reusing it" arm's
#                        upstream correction (#6095/#6100)
#   registered-worktree  the "worktree directory already exists, registered
#                        with git" fast path's upstream correction + drift
#                        report (#6257/#6291)
#
# It exists for exactly one consumer:
# `loom-daemon/tests/worktree_upstream_differential.rs`.
#
# WHY A FROZEN COPY RATHER THAN THE LIVE SOURCE
#
# Both blocks were inline in `worktree.sh`'s main body — never functions — so
# there is nothing a differential harness could `source` and call even before
# the port, and after it there is no shell implementation left in the tree at
# all. Reading them out of git history instead would pin the test to a moving
# ref.
#
# DO NOT "FIX" ANYTHING HERE. A wart in this file is the point: it is the
# behaviour the port is being compared against. In particular:
#
#   - `git branch --set-upstream-to=…` has its stderr redirected but NOT its
#     stdout, so its `branch 'X' set up to track 'origin/X'.` confirmation is
#     part of the retired output. The port reproduces that by inheriting fd 1.
#   - the `$JSON_OUTPUT` gate wraps only the `print_*` calls, never the git
#     commands — under `--json` the retired code still fetched and still
#     corrected the upstream, it only stopped narrating.
#   - the three-line uncommitted hint block's comment padding is column-
#     aligned by hand; those spaces are output.
#
# The two `print_*` helpers are copied verbatim from `worktree.sh` (they are
# the only helpers these blocks use). `$local_uncommitted` is a caller-supplied
# string in the original — the `git status --porcelain` output the fast path
# read BEFORE the fetch — and only its emptiness is ever tested, so it arrives
# here as an argument for the same reason the port takes `--uncommitted`.
#
# Usage: worktree-upstream-retired.sh <arm> <repo> <branch> <json-output> \
#                                     [<issue>] [<uncommitted>]
#
# <arm> is `local-branch` or `registered-worktree`. `local-branch` ran with the
# main workspace as cwd and no `git -C`; this wrapper `cd`s there to reproduce
# that exactly. `registered-worktree` addressed the worktree through `-C
# "$WORKTREE_PATH"` from that same cwd, so <repo> is the worktree there.

set -uo pipefail

ARM="${1:?arm}"
REPO="${2:?repo}"
BRANCH_NAME="${3:?branch}"
JSON_OUTPUT="${4:?json-output}"
ISSUE_NUMBER="${5:-}"
local_uncommitted="${6:-}"

# ===== BEGIN FROZEN HELPERS (worktree.sh lines 120-142) ======================
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}
# ===== END FROZEN HELPERS ====================================================

if [[ "$ARM" == "local-branch" ]]; then
    cd "$REPO" || exit 0
    # ===== BEGIN FROZEN BODY A (worktree.sh "Check if branch already exists"
    # reuse arm, #6095/#6100) =================================================
    git fetch origin "$BRANCH_NAME" 2>/dev/null || true
    if git show-ref --verify --quiet "refs/remotes/origin/$BRANCH_NAME"; then
        current_upstream="$(git rev-parse --abbrev-ref "$BRANCH_NAME@{u}" 2>/dev/null || true)"
        if [[ "$current_upstream" != "origin/$BRANCH_NAME" ]]; then
            if [[ "$JSON_OUTPUT" != "true" ]]; then if [[ -n "$current_upstream" ]]; then print_warning "Branch '$BRANCH_NAME' was tracking '$current_upstream' - correcting to 'origin/$BRANCH_NAME'"; else print_info "Branch '$BRANCH_NAME' has no upstream - setting it to 'origin/$BRANCH_NAME'"; fi; fi
            git branch --set-upstream-to="origin/$BRANCH_NAME" "$BRANCH_NAME" 2>/dev/null || true
        fi
    fi
    # ===== END FROZEN BODY A =================================================
    exit 0
fi

WORKTREE_PATH="$REPO"
# ===== BEGIN FROZEN BODY B (worktree.sh "worktree already exists, registered
# with git" fast path, #6257/#6291) ===========================================
git -C "$WORKTREE_PATH" fetch origin "$BRANCH_NAME" 2>/dev/null || true
if git -C "$WORKTREE_PATH" show-ref --verify --quiet "refs/remotes/origin/$BRANCH_NAME"; then
    wt_current_upstream="$(git -C "$WORKTREE_PATH" rev-parse --abbrev-ref "$BRANCH_NAME@{u}" 2>/dev/null || true)"
    if [[ "$wt_current_upstream" != "origin/$BRANCH_NAME" ]]; then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            if [[ -n "$wt_current_upstream" ]]; then
                print_warning "Worktree branch '$BRANCH_NAME' was tracking '$wt_current_upstream' - correcting to 'origin/$BRANCH_NAME'"
            else
                print_info "Worktree branch '$BRANCH_NAME' has no upstream - setting it to 'origin/$BRANCH_NAME'"
            fi
        fi
        git -C "$WORKTREE_PATH" branch --set-upstream-to="origin/$BRANCH_NAME" "$BRANCH_NAME" 2>/dev/null || true
    fi

    wt_head_sha="$(git -C "$WORKTREE_PATH" rev-parse HEAD 2>/dev/null || true)"
    wt_origin_tip="$(git -C "$WORKTREE_PATH" rev-parse "origin/$BRANCH_NAME" 2>/dev/null || true)"
    if [[ -n "$wt_head_sha" && -n "$wt_origin_tip" && "$wt_head_sha" != "$wt_origin_tip" ]] && \
       git -C "$WORKTREE_PATH" merge-base --is-ancestor "$wt_head_sha" "$wt_origin_tip" 2>/dev/null; then
        # Local HEAD is a strict ancestor of the branch's pushed tip -
        # i.e. genuinely behind (not just diverged/ahead with unpushed
        # local commits, which is expected and not drift).
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_warning "Worktree HEAD ($wt_head_sha) is behind the pushed tip of branch '$BRANCH_NAME' ($wt_origin_tip) - this worktree may be stale"
            if [[ -n "$local_uncommitted" ]]; then
                print_warning "Worktree also has uncommitted changes - resolve before evaluating/building on it:"
                print_info "  ./.loom/scripts/worktree.sh snapshot $ISSUE_NUMBER --include-untracked   # save WIP"
                print_info "  git -C $WORKTREE_PATH checkout -- .                                       # clear tracked working-tree drift"
                print_info "  git -C $WORKTREE_PATH pull --ff-only                                      # resync to origin/$BRANCH_NAME"
            else
                print_info "  git -C $WORKTREE_PATH pull --ff-only   # resync to origin/$BRANCH_NAME"
            fi
        fi
    fi
fi
# ===== END FROZEN BODY B =====================================================
exit 0
