#!/usr/bin/env bash
# Post-worktree hook: provide loom-daemon binary for Tauri compatibility
#
# Called by worktree.sh after creating a new worktree.
# Arguments: $1=worktree_path  $2=branch_name  $3=issue_number
# Working directory: the new worktree
#
# Copies loom-daemon from the main workspace's target/release/ instead of
# rebuilding from scratch. This avoids cargo lock contention and minutes-long
# release builds that block parallel worktrees.
#
# Falls back to building only if the main workspace binary doesn't exist.

set -euo pipefail

WORKTREE_PATH="${1:?worktree path required}"

# Only proceed if the worktree has a Cargo workspace with loom-daemon
if [[ ! -f "$WORKTREE_PATH/Cargo.toml" ]]; then
    exit 0
fi

if ! grep -q 'loom-daemon' "$WORKTREE_PATH/Cargo.toml" 2>/dev/null; then
    exit 0
fi

# Skip if the binary already exists (e.g., reusing an existing worktree)
if [[ -x "$WORKTREE_PATH/target/release/loom-daemon" ]]; then
    echo "  loom-daemon binary already exists, skipping"
    exit 0
fi

# Find the main workspace (parent of .loom/worktrees/)
MAIN_WORKSPACE="$(cd "$WORKTREE_PATH" && git rev-parse --git-common-dir 2>/dev/null | xargs dirname)"
MAIN_BINARY="$MAIN_WORKSPACE/target/release/loom-daemon"

# Try to copy from main workspace first (instant, no cargo lock contention)
if [[ -x "$MAIN_BINARY" ]]; then
    mkdir -p "$WORKTREE_PATH/target/release"
    if cp "$MAIN_BINARY" "$WORKTREE_PATH/target/release/loom-daemon"; then
        echo "  loom-daemon copied from main workspace (skipped rebuild)"
        exit 0
    fi
fi

# Fallback: build if main binary doesn't exist and cargo is available
if ! command -v cargo &>/dev/null; then
    echo "  cargo not found and no main workspace binary, skipping loom-daemon setup"
    exit 0
fi

echo "  Building loom-daemon (release) for Tauri compatibility..."
echo "  (main workspace binary not found at $MAIN_BINARY)"
if cargo build --release -p loom-daemon --manifest-path "$WORKTREE_PATH/Cargo.toml" 2>&1; then
    echo "  loom-daemon build complete"
else
    echo "  loom-daemon build failed (non-fatal, worktree still usable)"
fi

# Restore Cargo.lock — the build output is in target/ (gitignored),
# but cargo may update the lockfile which confuses shepherd diagnostics.
git -C "$WORKTREE_PATH" checkout -- Cargo.lock 2>/dev/null || true
