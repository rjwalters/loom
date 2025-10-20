#!/bin/bash
# Loom Cleanup Script - Remove build artifacts and orphaned worktrees

set -e  # Exit on error

echo "🧹 Loom Cleanup"
echo ""

# Track if we're in main workspace
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Clean Rust build artifacts
if [ -d "$PROJECT_ROOT/target" ]; then
  SIZE=$(du -sh "$PROJECT_ROOT/target" 2>/dev/null | cut -f1 || echo "unknown")
  echo "Removing target/ ($SIZE)"
  rm -rf "$PROJECT_ROOT/target"
  echo "✓ Removed target/"
else
  echo "ℹ No target/ directory found"
fi

echo ""

# Clean node_modules
if [ -d "$PROJECT_ROOT/node_modules" ]; then
  SIZE=$(du -sh "$PROJECT_ROOT/node_modules" 2>/dev/null | cut -f1 || echo "unknown")
  echo "Removing node_modules/ ($SIZE)"
  rm -rf "$PROJECT_ROOT/node_modules"
  echo "✓ Removed node_modules/"
else
  echo "ℹ No node_modules/ directory found"
fi

echo ""

# Clean orphaned worktrees
echo "Checking for orphaned worktrees..."
cd "$PROJECT_ROOT"

# Show what would be pruned
PRUNE_OUTPUT=$(git worktree prune --dry-run --verbose 2>&1 || true)

if [ -n "$PRUNE_OUTPUT" ]; then
  echo "$PRUNE_OUTPUT"
  echo ""
  read -p "Remove orphaned worktrees? (y/N) " -n 1 -r
  echo
  if [[ $REPLY =~ ^[Yy]$ ]]; then
    git worktree prune --verbose
    echo "✓ Orphaned worktrees removed"
  else
    echo "ℹ Skipped worktree cleanup"
  fi
else
  echo "✓ No orphaned worktrees found"
fi

echo ""
echo "✅ Cleanup complete!"
echo ""
echo "To restore dependencies, run:"
echo "  pnpm install"
