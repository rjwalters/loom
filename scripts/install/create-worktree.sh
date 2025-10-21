#!/usr/bin/env bash
# Create git worktree for Loom installation

set -euo pipefail

TARGET_PATH="${1:-.}"
ISSUE_NUMBER="${2:-}"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

error() {
  echo -e "${RED}✗ Error: $*${NC}" >&2
  exit 1
}

info() {
  echo -e "${BLUE}ℹ $*${NC}" >&2
}

success() {
  echo -e "${GREEN}✓ $*${NC}" >&2
}

if [[ -z "$ISSUE_NUMBER" ]]; then
  error "Issue number required as second argument"
fi

cd "$TARGET_PATH"

# Ensure .loom/worktrees directory exists
mkdir -p .loom/worktrees

WORKTREE_PATH=".loom/worktrees/issue-${ISSUE_NUMBER}"
BRANCH_NAME="feature/loom-installation"

info "Creating worktree for issue #${ISSUE_NUMBER}..."

# Get current branch to base from
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
info "Branching from: ${CURRENT_BRANCH}"

# Clean up any existing worktree
if [[ -d "$WORKTREE_PATH" ]]; then
  info "Removing existing worktree: $WORKTREE_PATH"
  git worktree remove "$WORKTREE_PATH" --force 2>/dev/null || true
fi

# Clean up any existing local branch
if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
  info "Removing existing local branch: $BRANCH_NAME"
  git branch -D "$BRANCH_NAME" 2>/dev/null || true
fi

# Create worktree (will create new local branch from current branch)
git worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$CURRENT_BRANCH" || \
  error "Failed to create worktree"

success "Worktree created: $WORKTREE_PATH"

# Output the worktree path (stdout, so it can be captured by caller)
echo "$WORKTREE_PATH"
