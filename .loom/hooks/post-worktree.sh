#!/usr/bin/env bash
# Post-worktree hook: pre-build loom-daemon binary for Tauri compatibility
#
# Called by worktree.sh after creating a new worktree.
# Arguments: $1=worktree_path  $2=branch_name  $3=issue_number
# Working directory: the new worktree
#
# Builds loom-daemon in release mode so that src-tauri/tauri.conf.json's
# externalBin reference ("../target/release/loom-daemon") is satisfied.
# This enables full `check:ci` and E2E tests in worktrees.

set -euo pipefail

WORKTREE_PATH="${1:?worktree path required}"

# Only build if the worktree has a Cargo workspace with loom-daemon
if [[ ! -f "$WORKTREE_PATH/Cargo.toml" ]]; then
    exit 0
fi

if ! grep -q 'loom-daemon' "$WORKTREE_PATH/Cargo.toml" 2>/dev/null; then
    exit 0
fi

# Skip if the binary already exists (e.g., reusing an existing worktree)
if [[ -x "$WORKTREE_PATH/target/release/loom-daemon" ]]; then
    echo "  loom-daemon binary already exists, skipping build"
    exit 0
fi

# Check that cargo is available
if ! command -v cargo &>/dev/null; then
    echo "  cargo not found, skipping loom-daemon build"
    exit 0
fi

echo "  Building loom-daemon (release) for Tauri compatibility..."
if cargo build --release -p loom-daemon --manifest-path "$WORKTREE_PATH/Cargo.toml" 2>&1; then
    echo "  loom-daemon build complete"
else
    echo "  loom-daemon build failed (non-fatal, worktree still usable)"
    exit 0
fi
