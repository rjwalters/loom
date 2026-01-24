#!/bin/bash
# Worktree helper script for Loom quickstart API
# Usage: ./.loom/scripts/worktree.sh <issue-number>

set -e

ISSUE_NUMBER="$1"
WORKTREE_DIR=".loom/worktrees/issue-$ISSUE_NUMBER"
BRANCH_NAME="feature/issue-$ISSUE_NUMBER"

if [ -z "$ISSUE_NUMBER" ]; then
  echo "Usage: $0 <issue-number>"
  echo "Example: $0 42"
  exit 1
fi

# Check if we're already in a worktree
if git rev-parse --is-inside-work-tree &>/dev/null; then
  TOPLEVEL=$(git rev-parse --show-toplevel)
  if [ -f "$TOPLEVEL/.git" ]; then
    echo "Error: Already in a worktree. Navigate to main repository first."
    exit 1
  fi
fi

# Create the worktree
echo "Creating worktree for issue #$ISSUE_NUMBER..."
git worktree add "$WORKTREE_DIR" -b "$BRANCH_NAME" main

echo ""
echo "Worktree created successfully!"
echo ""
echo "Next steps:"
echo "  cd $WORKTREE_DIR"
echo "  pnpm install"
echo "  pnpm dev"
echo ""
echo "When done:"
echo "  git add -A"
echo "  git commit -m 'Your message'"
echo "  git push -u origin $BRANCH_NAME"
echo "  gh pr create --label 'loom:review-requested'"
