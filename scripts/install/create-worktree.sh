#!/usr/bin/env bash
# Create git worktree for Loom installation

set -euo pipefail

TARGET_PATH="${1:-.}"

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

cd "$TARGET_PATH"

# Ensure .loom/worktrees directory exists
mkdir -p .loom/worktrees

WORKTREE_PATH=".loom/worktrees/loom-installation"
BASE_BRANCH_NAME="feature/loom-installation"

info "Creating worktree for Loom installation..."

# Detect the default branch (usually 'main' or 'master')
# Strategy: Try multiple detection methods and validate the result

# Method 1: Try git symbolic-ref (but this can be stale)
DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "")

# Method 2: If symbolic-ref returned a value, validate it exists on remote
if [[ -n "$DEFAULT_BRANCH" ]]; then
  # Fetch to update remote refs first
  git fetch origin --prune 2>/dev/null || true

  # Check if the branch actually exists on remote
  if ! git show-ref --verify --quiet "refs/remotes/origin/${DEFAULT_BRANCH}"; then
    info "Detected branch '${DEFAULT_BRANCH}' from symbolic-ref but it doesn't exist on remote"
    DEFAULT_BRANCH=""
  fi
fi

# Method 3: If still empty, check for common branch names on remote
if [[ -z "$DEFAULT_BRANCH" ]]; then
  if git show-ref --verify --quiet refs/remotes/origin/main; then
    DEFAULT_BRANCH="main"
  elif git show-ref --verify --quiet refs/remotes/origin/master; then
    DEFAULT_BRANCH="master"
  fi
fi

# Method 4: Fall back to local branches
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
# Fetch the branch from origin first
info "Fetching latest changes from origin/${DEFAULT_BRANCH}..."
git fetch origin "${DEFAULT_BRANCH}" 2>/dev/null || true

# Verify the branch exists (locally or as remote ref)
# Prefer origin/DEFAULT_BRANCH as the base to ensure we have the latest
if git show-ref --verify --quiet "refs/remotes/origin/${DEFAULT_BRANCH}"; then
  BASE_BRANCH="origin/${DEFAULT_BRANCH}"
  info "Branching from: ${BASE_BRANCH}"
elif git show-ref --verify --quiet "refs/heads/${DEFAULT_BRANCH}"; then
  BASE_BRANCH="${DEFAULT_BRANCH}"
  info "Branching from: ${BASE_BRANCH}"
else
  # Fall back to HEAD if we can't find the branch
  BASE_BRANCH="HEAD"
  info "Could not find ${DEFAULT_BRANCH}, branching from HEAD"
fi

# Clean up any existing worktree
if [[ -d "$WORKTREE_PATH" ]]; then
  info "Removing existing worktree: $WORKTREE_PATH"
  git worktree remove "$WORKTREE_PATH" --force 2>/dev/null || true
fi

# Prune any stale worktree metadata (defensive: handles incomplete uninstall)
info "Pruning stale worktree metadata..."
git worktree prune 2>/dev/null || true

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
# Note: For PR target, always use DEFAULT_BRANCH (not BASE_BRANCH which might be HEAD)
# Strip origin/ prefix if present
TARGET_BRANCH="${DEFAULT_BRANCH#origin/}"
printf "%s|%s|%s" "${WORKTREE_PATH}" "${BRANCH_NAME}" "${TARGET_BRANCH}"
