#!/usr/bin/env bash
# FROZEN COPY of worktree.sh's `_handle_feature_branch_in_main_worktree` as it
# stood immediately before #8195 slice 7 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/worktree_branch_conflict_differential.rs` can keep
# comparing the Rust against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the function out of the
# live worktree.sh stopped being possible the instant that file started
# delegating.
#
# WHAT IS FROZEN AND WHAT IS NOT
#
# The function body is byte-for-byte the retired code, variable names and
# all, including its `if [[ "$JSON_OUTPUT" != "true" ]]` gate around every
# message (`print_error` included). `print_error`/`print_success`/
# `print_info`/`print_warning` are re-declared here rather than sourced from
# the live script's colour block, for the same reason `worktree-link-retired.sh`
# does: if the live script's colours ever change, this fixture must keep
# modelling the RETIRED shell.
#
# ONE DELIBERATE INPUT CHANGE, not a behaviour change: the retired function
# derived `$main_workspace` itself via `git rev-parse --git-common-dir` (run in
# whatever the CALLING PROCESS's cwd happened to be) plus `dirname`. By the
# time `_try_worktree_add` ran, `worktree.sh` had already `cd`'d into the main
# workspace and captured it once as `$WORKTREE_REPO_ROOT` — so that derivation
# always answered the same directory the script already had in a variable.
# This fixture (like the Rust port) takes that value as an argument instead of
# re-deriving it, which sidesteps a real problem for a test harness (a
# `git rev-parse` invocation's answer depends on the test process's cwd, not on
# anything the corpus controls) without changing what either side computes.
#
# Usage: worktree-branch-conflict-retired.sh <branch> <default-branch> <issue> <repo-root> <json 0|1>
# Reads the captured `git worktree add` stderr on stdin.

set -e

BRANCH="$1"
DEFAULT_BRANCH="$2"
ISSUE_NUMBER="$3"
main_workspace="$4"
JSON_OUTPUT="false"
[[ "$5" == "1" ]] && JSON_OUTPUT="true"

error_output="$(cat)"

# The retired colour block.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

print_error() {
    echo -e "${RED}ERROR: $1${NC}" >&2
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

# --- Below is `_handle_feature_branch_in_main_worktree`, frozen verbatim
# --- except for `branch`/`main_workspace` arriving as arguments instead of a
# --- function's own locals (this file has no enclosing function to be one).

if ! echo "$error_output" | grep -q "is already used by worktree at"; then
    exit 1 # Not this error — caller should fail normally
fi

conflict_path=$(echo "$error_output" | grep -o "is already used by worktree at '[^']*'" | sed "s/is already used by worktree at '//;s/'$//")

if [[ -z "$conflict_path" ]]; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_error "Cannot create worktree: branch '$BRANCH' is already checked out in another worktree."
        echo ""
        echo "  The branch is in use elsewhere. To free it, find the worktree with:"
        echo "    git worktree list"
        echo "  Then switch that worktree to $DEFAULT_BRANCH:"
        echo "    cd <worktree-path> && git checkout $DEFAULT_BRANCH"
    fi
    exit 0
fi

abs_conflict=$(cd "$conflict_path" 2>/dev/null && pwd) || abs_conflict="$conflict_path"
abs_main=$(cd "$main_workspace" 2>/dev/null && pwd) || abs_main="$main_workspace"

if [[ "$abs_conflict" != "$abs_main" ]]; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_error "Cannot create worktree for branch '$BRANCH':"
        echo "  Branch is already checked out at: $conflict_path"
        echo ""
        echo "  To fix:"
        echo "    cd $conflict_path && git checkout $DEFAULT_BRANCH"
    fi
    exit 0
fi

uncommitted=$(git -C "$abs_conflict" status --porcelain 2>/dev/null)

if [[ -n "$uncommitted" ]]; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_error "Cannot create worktree for issue #$ISSUE_NUMBER: branch '$BRANCH'"
        echo "  is already checked out at '$abs_conflict' (main worktree)."
        echo ""
        echo "  The main worktree has uncommitted changes — cannot auto-switch."
        echo "  To fix manually:"
        echo "    cd $abs_conflict"
        echo "    git stash  # or commit your changes"
        echo "    git checkout $DEFAULT_BRANCH"
        echo "  Then rerun: ./.loom/scripts/worktree.sh $ISSUE_NUMBER"
    fi
    exit 0
fi

if [[ "$JSON_OUTPUT" != "true" ]]; then
    print_warning "Branch '$BRANCH' is checked out in the main worktree."
    print_info "Main worktree is clean — auto-switching to $DEFAULT_BRANCH branch..."
fi

if git -C "$abs_conflict" checkout "$DEFAULT_BRANCH" 2>/dev/null; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_success "Main worktree switched to $DEFAULT_BRANCH branch"
    fi
    exit 2
else
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_error "Failed to switch main worktree to $DEFAULT_BRANCH branch."
        echo "  To fix manually:"
        echo "    cd $abs_conflict && git checkout $DEFAULT_BRANCH"
        echo "  Then rerun: ./.loom/scripts/worktree.sh $ISSUE_NUMBER"
    fi
    exit 0
fi
