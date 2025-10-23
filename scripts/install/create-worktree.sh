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

# Detect the default branch (usually 'main' or 'master')
# First try to get it from the remote HEAD
DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "")

# If that fails, check if 'main' or 'master' exists locally
if [[ -z "$DEFAULT_BRANCH" ]]; then
  if git show-ref --verify --quiet refs/heads/main; then
    DEFAULT_BRANCH="main"
  elif git show-ref --verify --quiet refs/heads/master; then
    DEFAULT_BRANCH="master"
  else
    # Fall back to current branch if we can't determine default
    DEFAULT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    info "Could not detect default branch, using current branch"
  fi
fi

# Ensure we're branching from the latest state
if [[ "$DEFAULT_BRANCH" == "main" ]] || [[ "$DEFAULT_BRANCH" == "master" ]]; then
  info "Fetching latest changes from origin/${DEFAULT_BRANCH}..."
  git fetch origin "${DEFAULT_BRANCH}:${DEFAULT_BRANCH}" 2>/dev/null || true
fi

info "Branching from: ${DEFAULT_BRANCH}"
BASE_BRANCH="$DEFAULT_BRANCH"

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
    git branch -D "$BRANCH_NAME" >/dev/null 2>&1 || true
  fi

  # Try to create worktree with this branch name
  if git worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$BASE_BRANCH" >&2 2>&1; then
    # Success! Branch name is available
    break
  fi

  # Branch exists (likely remote), try next suffix
  info "Branch '$BRANCH_NAME' already exists, trying alternative..."
  BRANCH_NAME="${BASE_BRANCH_NAME}-${SUFFIX}"
  SUFFIX=$((SUFFIX + 1))

  # Safety limit to prevent infinite loop
  if [[ $SUFFIX -gt 10 ]]; then
    error "Could not find available branch name after 10 attempts"
  fi
done

if [[ "$BRANCH_NAME" != "$BASE_BRANCH_NAME" ]]; then
  info "Using alternative branch name: $BRANCH_NAME"
fi

success "Worktree created: $WORKTREE_PATH"
success "Branch name: $BRANCH_NAME"

# Output the worktree path, branch name, and base branch (stdout, so it can be captured by caller)
# Format: WORKTREE_PATH|BRANCH_NAME|BASE_BRANCH
echo "${WORKTREE_PATH}|${BRANCH_NAME}|${BASE_BRANCH}"
