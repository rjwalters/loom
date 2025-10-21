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
BASE_BRANCH_NAME="feature/loom-installation"

info "Creating worktree for issue #${ISSUE_NUMBER}..."

# Get current branch to base from
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
info "Branching from: ${CURRENT_BRANCH}"

# Clean up any existing worktree
if [[ -d "$WORKTREE_PATH" ]]; then
  info "Removing existing worktree: $WORKTREE_PATH"
  git worktree remove "$WORKTREE_PATH" --force 2>/dev/null || true
fi

# Find an available branch name (handle case where remote branch exists)
BRANCH_NAME="$BASE_BRANCH_NAME"
SUFFIX=2

while true; do
  # Clean up any existing local branch with this name
  if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
    info "Removing existing local branch: $BRANCH_NAME"
    git branch -D "$BRANCH_NAME" 2>/dev/null || true
  fi

  # Try to create worktree with this branch name
  if git worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$CURRENT_BRANCH" 2>/dev/null; then
    # Success! Branch name is available
    break
  fi

  # Branch exists (likely remote), try next suffix
  info "Branch '$BRANCH_NAME' already exists, trying alternative..."
  BRANCH_NAME="${BASE_BRANCH_NAME}-${SUFFIX}"
  SUFFIX=$((SUFFIX + 1))

  # Safety limit to prevent infinite loop
  if [[ $SUFFIX -gt 100 ]]; then
    error "Could not find available branch name after 100 attempts"
  fi
done

if [[ "$BRANCH_NAME" != "$BASE_BRANCH_NAME" ]]; then
  info "Using alternative branch name: $BRANCH_NAME"
fi

success "Worktree created: $WORKTREE_PATH"
success "Branch name: $BRANCH_NAME"

# Output the worktree path and branch name (stdout, so it can be captured by caller)
# Format: WORKTREE_PATH|BRANCH_NAME
echo "${WORKTREE_PATH}|${BRANCH_NAME}"
