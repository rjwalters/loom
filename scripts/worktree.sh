#!/bin/bash

# Loom Worktree Helper Script
# Safely creates and manages git worktrees for agent development
#
# Usage:
#   pnpm worktree <issue-number>           # Create worktree for issue
#   pnpm worktree <issue-number> <branch>  # Create worktree with custom branch name
#   pnpm worktree --check                  # Check if currently in a worktree
#   pnpm worktree --help                   # Show help

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print colored output
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

# Function to check if we're in a worktree
check_if_in_worktree() {
    local git_dir=$(git rev-parse --git-common-dir 2>/dev/null)
    local work_dir=$(git rev-parse --show-toplevel 2>/dev/null)

    if [[ "$git_dir" != "$work_dir/.git" ]]; then
        return 0  # In a worktree
    else
        return 1  # In main working directory
    fi
}

# Function to get current worktree info
get_worktree_info() {
    if check_if_in_worktree; then
        local current_dir=$(pwd)
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

# Function to show help
show_help() {
    cat << EOF
Loom Worktree Helper

This script helps AI agents safely create and manage git worktrees.

Usage:
  pnpm worktree <issue-number>           Create worktree for issue
  pnpm worktree <issue-number> <branch>  Create worktree with custom branch
  pnpm worktree --check                  Check if in a worktree
  pnpm worktree --help                   Show this help

Examples:
  pnpm worktree 42
    Creates: .loom/worktrees/issue-42
    Branch: feature/issue-42

  pnpm worktree 42 fix-bug
    Creates: .loom/worktrees/issue-42
    Branch: feature/fix-bug

  pnpm worktree --check
    Shows current worktree status

Safety Features:
  ✓ Detects if already in a worktree
  ✓ Uses sandbox-safe path (.loom/worktrees/)
  ✓ Automatically creates branch from main
  ✓ Prevents nested worktrees
  ✓ Shows clear error messages

Notes:
  - All worktrees are created in .loom/worktrees/ (gitignored)
  - Branch names automatically prefixed with 'feature/'
  - After creation, cd into the worktree to start working
  - To return to main: cd /path/to/loom && git checkout main
EOF
}

# Parse arguments
if [[ $# -eq 0 ]] || [[ "$1" == "--help" ]] || [[ "$1" == "-h" ]]; then
    show_help
    exit 0
fi

if [[ "$1" == "--check" ]]; then
    get_worktree_info
    exit $?
fi

# Main worktree creation logic
ISSUE_NUMBER="$1"
CUSTOM_BRANCH="$2"

# Validate issue number
if ! [[ "$ISSUE_NUMBER" =~ ^[0-9]+$ ]]; then
    print_error "Issue number must be numeric (got: '$ISSUE_NUMBER')"
    echo ""
    echo "Usage: pnpm worktree <issue-number> [branch-name]"
    exit 1
fi

# Check if already in a worktree
if check_if_in_worktree; then
    print_error "You are already in a worktree!"
    echo ""
    get_worktree_info
    echo ""
    print_info "To create a new worktree:"
    echo "  1. Return to main: cd \$(git rev-parse --show-toplevel)/../.. && git checkout main"
    echo "  2. Then run: pnpm worktree $ISSUE_NUMBER"
    exit 1
fi

# Determine branch name
if [[ -n "$CUSTOM_BRANCH" ]]; then
    BRANCH_NAME="feature/$CUSTOM_BRANCH"
else
    BRANCH_NAME="feature/issue-$ISSUE_NUMBER"
fi

# Worktree path
WORKTREE_PATH=".loom/worktrees/issue-$ISSUE_NUMBER"

# Check if worktree already exists
if [[ -d "$WORKTREE_PATH" ]]; then
    print_warning "Worktree already exists at: $WORKTREE_PATH"

    # Check if it's registered with git
    if git worktree list | grep -q "$WORKTREE_PATH"; then
        print_info "Worktree is registered with git"
        echo ""
        print_info "To use this worktree: cd $WORKTREE_PATH"
        exit 0
    else
        print_error "Directory exists but is not a registered worktree"
        echo ""
        print_info "To fix this:"
        echo "  1. Remove the directory: rm -rf $WORKTREE_PATH"
        echo "  2. Run again: pnpm worktree $ISSUE_NUMBER"
        exit 1
    fi
fi

# Check if branch already exists
if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
    print_warning "Branch '$BRANCH_NAME' already exists"

    # Ask if we should use existing branch or create new one
    read -p "Use existing branch? (y/n) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        print_info "Please specify a different branch name:"
        echo "  pnpm worktree $ISSUE_NUMBER <custom-branch-name>"
        exit 1
    fi

    CREATE_ARGS=("$WORKTREE_PATH" "$BRANCH_NAME")
else
    # Create new branch from main
    CREATE_ARGS=("$WORKTREE_PATH" "-b" "$BRANCH_NAME" "main")
fi

# Create the worktree
print_info "Creating worktree..."
echo "  Path: $WORKTREE_PATH"
echo "  Branch: $BRANCH_NAME"
echo ""

if git worktree add "${CREATE_ARGS[@]}"; then
    print_success "Worktree created successfully!"
    echo ""
    print_info "Next steps:"
    echo "  cd $WORKTREE_PATH"
    echo "  # Do your work..."
    echo "  git add -A"
    echo "  git commit -m 'Your message'"
    echo "  git push -u origin $BRANCH_NAME"
    echo "  gh pr create"
else
    print_error "Failed to create worktree"
    exit 1
fi
