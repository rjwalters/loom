#!/bin/bash
set -e

echo "=== Testing Terminal Worktree Creation ==="
echo

# Get the workspace path (parent of .loom)
WORKSPACE_PATH="/Users/rwalters/GitHub/loom"
WORKTREES_DIR="$WORKSPACE_PATH/.loom/worktrees"

# Clean up any existing test worktrees
echo "Cleaning up existing test worktrees..."
for i in {1..5}; do
  if [ -d "$WORKTREES_DIR/terminal-$i" ]; then
    echo "  Removing $WORKTREES_DIR/terminal-$i"
    cd "$WORKSPACE_PATH"
    git worktree remove "$WORKTREES_DIR/terminal-$i" --force 2>/dev/null || true
    git branch -D "worktree/terminal-$i" 2>/dev/null || true
  fi
done

echo
echo "=== Test Results ==="
echo
echo "Verification steps:"
echo "1. After factory reset + start, check for terminal worktrees:"
echo "   ls -la $WORKTREES_DIR"
echo
echo "2. Verify each worktree has CLAUDE.md with role content:"
echo "   for i in {1..5}; do"
echo "     echo \"=== terminal-\$i ===\""
echo "     head -5 $WORKTREES_DIR/terminal-\$i/CLAUDE.md"
echo "   done"
echo
echo "3. Verify terminals are running in worktrees:"
echo "   tmux -L loom list-sessions"
echo
echo "Manual test: Use the Loom app to trigger factory reset + start"
echo "Then run the verification steps above."
