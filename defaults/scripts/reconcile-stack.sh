#!/usr/bin/env bash
# Loom Stacked-PR Reconciliation (issue #3729, stacked-PR v1)
#
# Turns the manual git surgery an operator performs after a stacked parent PR
# squash-merges into one command. This is the v1 manual-but-scripted
# reconciliation step: merge-pr.sh is intentionally NOT modified in v1, and no
# automatic merge-ordering guard is added. The operator runs this by hand AFTER
# confirming the parent branch has squash-merged to the default branch.
#
# Usage:
#   ./.loom/scripts/reconcile-stack.sh <child-pr> <parent-branch> [options]
#
# Example (the live #3725-on-#3726 incident, PR #3727):
#   ./.loom/scripts/reconcile-stack.sh 3727 feature/issue-3726
#
# What it does (equivalent to the three-command manual workaround):
#   git rebase --onto <default-branch> <parent-branch> <child-branch>
#   git push --force-with-lease
#   gh pr edit <child-pr> --base <default-branch>
#
# The repo squash-merges (setup-repository-settings.sh: squash only), so after
# the parent squash-merges to the default branch as ONE commit, the child
# branch still carries the parent's ORIGINAL pre-squash commits. A naive base
# retarget (child base -> default) then re-shows the parent's entire diff. The
# `git rebase --onto` replays ONLY the child's own commits onto the default
# branch, stripping the parent's now-squashed commits, before retargeting.
#
# Safety:
#   - Uses --force-with-lease (NEVER a bare --force) so a concurrent push to the
#     child branch aborts the rebase rather than clobbering it.
#   - --dry-run prints every command without executing.
#   - Refuses to run with a dirty working tree (a rebase would fail confusingly).
#
# Options:
#   --dry-run   Print the commands that would run without executing them.
#   --help,-h   Show this help.
#
# Exit codes:
#   0 = reconciled (or dry-run printed)
#   1 = usage / precondition failure
#   2 = a git/gh step failed (rebase conflict, push rejected, retarget failed)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Colors (skip when not a TTY).
if [[ -t 2 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; NC=''
fi
err()  { echo -e "${RED}ERROR: $1${NC}" >&2; }
ok()   { echo -e "${GREEN}✓ $1${NC}" >&2; }
info() { echo -e "${BLUE}ℹ $1${NC}" >&2; }
warn() { echo -e "${YELLOW}⚠ $1${NC}" >&2; }

show_help() {
    sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

DRY_RUN=false
CHILD_PR=""
PARENT_BRANCH=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=true; shift ;;
        --help|-h) show_help; exit 0 ;;
        --*) err "Unknown flag: $1"; exit 1 ;;
        *)
            if [[ -z "$CHILD_PR" ]]; then
                CHILD_PR="$1"
            elif [[ -z "$PARENT_BRANCH" ]]; then
                PARENT_BRANCH="$1"
            else
                err "Unexpected argument: $1"; exit 1
            fi
            shift
            ;;
    esac
done

if [[ -z "$CHILD_PR" || -z "$PARENT_BRANCH" ]]; then
    err "Usage: reconcile-stack.sh <child-pr> <parent-branch> [--dry-run]"
    echo "  Example: reconcile-stack.sh 3727 feature/issue-3726" >&2
    exit 1
fi

if ! [[ "$CHILD_PR" =~ ^[0-9]+$ ]]; then
    err "Child PR must be numeric (got: '$CHILD_PR')"
    exit 1
fi

# Resolve the repo's default branch (the rebase --onto target) offline, honoring
# a LOOM_DEFAULT_BRANCH override. Same resolver worktree.sh uses.
DEFAULT_BRANCH="main"
if [[ -f "$SCRIPT_DIR/lib/default-branch.sh" ]]; then
    # shellcheck source=lib/default-branch.sh
    source "$SCRIPT_DIR/lib/default-branch.sh"
    if resolved="$(loom_default_branch 2>/dev/null)" && [[ -n "$resolved" ]]; then
        DEFAULT_BRANCH="$resolved"
    fi
fi

# Discover the child branch from the PR (GitHub via gh).
if ! command -v gh >/dev/null 2>&1; then
    err "gh CLI not found — required to resolve the child PR's head branch and retarget its base."
    exit 1
fi

info "Resolving head branch for child PR #$CHILD_PR..."
CHILD_BRANCH="$(gh pr view "$CHILD_PR" --json headRefName --jq '.headRefName' 2>/dev/null || true)"
if [[ -z "$CHILD_BRANCH" ]]; then
    err "Could not resolve the head branch for PR #$CHILD_PR (is the number correct and the PR open?)."
    exit 1
fi
info "Child branch: $CHILD_BRANCH"
info "Parent branch: $PARENT_BRANCH"
info "Rebase target (default branch): $DEFAULT_BRANCH"

# Best-effort advisory: has the parent branch squash-merged? If origin still has
# the parent branch, the operator likely hasn't merged it yet — warn but do not
# block (the operator asserts they've confirmed the merge).
git fetch origin "$DEFAULT_BRANCH" 2>/dev/null || true
if git show-ref --verify --quiet "refs/remotes/origin/$PARENT_BRANCH" 2>/dev/null; then
    warn "origin/$PARENT_BRANCH still exists — confirm the parent PR has squash-merged before reconciling."
    warn "(delete-branch-on-merge normally removes it once the parent merges.)"
fi

# Refuse on a dirty working tree — a rebase would fail confusingly.
if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    err "Working tree is dirty. Commit, stash, or discard changes before reconciling."
    exit 1
fi

run() {
    echo -e "${YELLOW}\$ $*${NC}" >&2
    if [[ "$DRY_RUN" == "true" ]]; then
        return 0
    fi
    "$@"
}

# 1. Replay ONLY the child's own commits onto the default branch, stripping the
#    parent's now-squashed pre-merge commits.
info "Step 1/3: rebase --onto $DEFAULT_BRANCH $PARENT_BRANCH $CHILD_BRANCH"
if ! run git rebase --onto "$DEFAULT_BRANCH" "$PARENT_BRANCH" "$CHILD_BRANCH"; then
    err "Rebase failed (likely a conflict). Resolve it, then re-run this script or finish manually:"
    echo "    git rebase --continue   # after resolving" >&2
    echo "    git push --force-with-lease" >&2
    echo "    gh pr edit $CHILD_PR --base $DEFAULT_BRANCH" >&2
    exit 2
fi

# 2. Publish the rewritten child branch. --force-with-lease (never bare --force)
#    so a concurrent push aborts rather than clobbers.
info "Step 2/3: push --force-with-lease"
if ! run git push --force-with-lease; then
    err "Force-with-lease push was rejected (someone else pushed to $CHILD_BRANCH). Fetch, review, and retry."
    exit 2
fi

# 3. Retarget the child PR's base to the default branch.
info "Step 3/3: gh pr edit $CHILD_PR --base $DEFAULT_BRANCH"
if ! run gh pr edit "$CHILD_PR" --base "$DEFAULT_BRANCH"; then
    err "Failed to retarget PR #$CHILD_PR base to $DEFAULT_BRANCH. Retarget it manually."
    exit 2
fi

if [[ "$DRY_RUN" == "true" ]]; then
    ok "Dry run complete — no changes made. Re-run without --dry-run to reconcile."
else
    ok "Reconciled: PR #$CHILD_PR now stacks only its own commits on $DEFAULT_BRANCH."
fi
