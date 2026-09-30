#!/usr/bin/env bash
# worktree-check-retired.sh — FROZEN copy of the shell this slice retired.
#
# `defaults/scripts/worktree.sh`'s in-worktree detection exactly as it stood
# immediately before #8195 slice 11 replaced it with a delegation to
# `loom-daemon worktree-check`:
#
#   check_if_in_worktree   the predicate
#   get_worktree_info      the `--check` verb's body
#   navigate               the create path's auto-navigation block, verbatim
#                          down to the `cd` and the two failure arms
#
# It exists for exactly one consumer:
# `loom-daemon/tests/worktree_check_differential.rs`.
#
# WHY A FROZEN COPY RATHER THAN THE LIVE SOURCE
#
# The predicate and `get_worktree_info` were functions, but the navigation block
# was inline in `worktree.sh`'s main body and is gone from the tree entirely
# after the port, so there is nothing left to `source`. Reading it out of git
# history instead would pin the test to a moving ref.
#
# DO NOT "FIX" ANYTHING HERE — the defect IS the thing being compared against:
#
#   - `check_if_in_worktree` compares `--git-common-dir`, which git answers
#     RELATIVE to the current directory whenever it can, against an ABSOLUTE
#     `$(git rev-parse --show-toplevel)/.git`. That is why it answers "in a
#     worktree" everywhere, including the primary clone, and why the harness's
#     assertions are DISAGREEMENTS rather than matches for three of the four
#     positions.
#   - `MAIN_WORKSPACE=$(dirname "$GIT_COMMON_DIR")` therefore produces `.` in
#     the primary clone (relative), and an absolute path in a real worktree.
#   - the `$JSON_OUTPUT` gate wraps only the `print_*` calls; the `cd` itself
#     and both `exit 1` arms run in either mode.
#
# The four `print_*` helpers and the `>&3` convention are copied verbatim from
# `worktree.sh` (fd 3 is the real stdout there; the caller opens it).
#
# Usage: worktree-check-retired.sh check              # the `--check` verb
#        worktree-check-retired.sh navigate           # the create-path block
# Environment: JSON_OUTPUT=true|false (default false)

set -e

JSON_OUTPUT="${JSON_OUTPUT:-false}"

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

if [[ "$JSON_OUTPUT" == "true" ]]; then
    exec 3>&1 1>&2
else
    exec 3>&1
fi

# --- retired: check_if_in_worktree -----------------------------------------
check_if_in_worktree() {
    local git_dir=$(git rev-parse --git-common-dir 2>/dev/null)
    local work_dir=$(git rev-parse --show-toplevel 2>/dev/null)

    if [[ "$git_dir" != "$work_dir/.git" ]]; then
        return 0  # In a worktree
    else
        return 1  # In main working directory
    fi
}

# --- retired: get_worktree_info --------------------------------------------
get_worktree_info() {
    if check_if_in_worktree; then
        local worktree_path=$(git rev-parse --show-toplevel)
        local branch=$(git rev-parse --abbrev-ref HEAD)

        echo "Current worktree:"
        echo "  Path: $worktree_path"
        echo "  Branch: $branch"
        return 0
    else
        echo "Not currently in a worktree (you're in the main working directory)"
        return 1
    fi
}

case "${1:-}" in
    check)
        get_worktree_info
        exit $?
        ;;
    navigate) ;;
    *)
        echo "worktree-check-retired.sh: unknown arm '${1:-}'" >&2
        exit 64
        ;;
esac

# --- retired: the create path's auto-navigation block ----------------------
if check_if_in_worktree; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_warning "Currently in a worktree, auto-navigating to main workspace..."
        echo ""
        get_worktree_info
        echo ""
    fi

    # Find the git root (common directory for all worktrees)
    GIT_COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null)
    if [[ -z "$GIT_COMMON_DIR" ]]; then
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            echo '{"error": "Failed to find git common directory"}' >&3
        else
            print_error "Failed to find git common directory"
        fi
        exit 1
    fi

    # The main workspace is the parent of .git (or the directory containing .git)
    MAIN_WORKSPACE=$(dirname "$GIT_COMMON_DIR")
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_info "Found main workspace: $MAIN_WORKSPACE"
    fi

    # Change to main workspace
    if cd "$MAIN_WORKSPACE" 2>/dev/null; then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_success "Switched to main workspace"
        fi
    else
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            echo '{"error": "Failed to change to main workspace", "mainWorkspace": "'"$MAIN_WORKSPACE"'"}' >&3
        else
            print_error "Failed to change to main workspace: $MAIN_WORKSPACE"
            print_info "Please manually run: cd $MAIN_WORKSPACE"
        fi
        exit 1
    fi
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        echo ""
    fi
fi

# The retired block left the caller's cwd as its only other observable. The
# harness reads it from this line, which the original did not need because the
# `cd` applied to the rest of the script.
echo "CWD=$(pwd -P)" >&3
