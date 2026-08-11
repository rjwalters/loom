#!/usr/bin/env bash
# Post-worktree hook: provide loom-daemon binary for the new worktree
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
#
# Issue #6013: Cargo's build output is NOT necessarily `<repo>/target/` -- a
# `CARGO_TARGET_DIR` env var or `build.target-dir` in `~/.cargo/config.toml`
# redirects it wholesale (issue #5922). A hardcoded
# `$MAIN_WORKSPACE/target/release/loom-daemon` lookup is therefore always
# "missing" on such a host, so every worktree fell through to a full
# `cargo build --release -p loom-daemon`, reintroducing the lock-contention
# rebuild storm that #2291 originally fixed. Resolve the real target dir the
# same way `scripts/cargo-target-dir.sh` does (CARGO_TARGET_DIR env ->
# `cargo metadata` -> `<root>/target` fallback) instead of assuming the
# default layout.

set -euo pipefail

WORKTREE_PATH="${1:?worktree path required}"

# Only proceed if the worktree has a Cargo workspace with loom-daemon
if [[ ! -f "$WORKTREE_PATH/Cargo.toml" ]]; then
    exit 0
fi

if ! grep -q 'loom-daemon' "$WORKTREE_PATH/Cargo.toml" 2>/dev/null; then
    exit 0
fi

# Find the main workspace (parent of .loom/worktrees/) and its target-dir
# resolution helper (#5922).
MAIN_WORKSPACE="$(cd "$WORKTREE_PATH" && git rev-parse --git-common-dir 2>/dev/null | xargs dirname)"
CARGO_TARGET_DIR_SCRIPT="$MAIN_WORKSPACE/scripts/cargo-target-dir.sh"

# Resolve the real (possibly redirected) target dir for a given workspace
# root. Falls back to the pre-#6013 hardcoded assumption (`<root>/target`) if
# the helper script itself is missing (e.g. a partial checkout) -- mirrors
# cargo-target-dir.sh's own internal fallback, so this degrades exactly to
# the old behavior rather than breaking a worktree creation outright.
resolve_target_dir() {
    local workspace_root="$1"
    if [[ -x "$CARGO_TARGET_DIR_SCRIPT" ]]; then
        "$CARGO_TARGET_DIR_SCRIPT" "$workspace_root" 2>/dev/null || echo "$workspace_root/target"
    else
        echo "$workspace_root/target"
    fi
}

WORKTREE_TARGET_DIR="$(resolve_target_dir "$WORKTREE_PATH")"
MAIN_TARGET_DIR="$(resolve_target_dir "$MAIN_WORKSPACE")"

WORKTREE_BINARY="$WORKTREE_TARGET_DIR/release/loom-daemon"
MAIN_BINARY="$MAIN_TARGET_DIR/release/loom-daemon"

# Skip if the binary already exists (e.g., reusing an existing worktree, or a
# redirected CARGO_TARGET_DIR shared verbatim across worktrees)
if [[ -x "$WORKTREE_BINARY" ]]; then
    echo "  loom-daemon binary already exists, skipping"
    exit 0
fi

# Try to copy from main workspace first (instant, no cargo lock contention)
if [[ -x "$MAIN_BINARY" ]]; then
    mkdir -p "$(dirname "$WORKTREE_BINARY")"
    if cp "$MAIN_BINARY" "$WORKTREE_BINARY"; then
        echo "  loom-daemon copied from main workspace (skipped rebuild)"
        exit 0
    fi
fi

# Fallback: build if main binary doesn't exist and cargo is available
if ! command -v cargo &>/dev/null; then
    echo "  cargo not found and no main workspace binary, skipping loom-daemon setup"
    exit 0
fi

echo "  Building loom-daemon (release)..."
echo "  (main workspace binary not found at $MAIN_BINARY)"
if cargo build --release -p loom-daemon --manifest-path "$WORKTREE_PATH/Cargo.toml" 2>&1; then
    echo "  loom-daemon build complete"
else
    echo "  loom-daemon build failed (non-fatal, worktree still usable)"
fi

# Restore Cargo.lock — the build output is in target/ (gitignored),
# but cargo may update the lockfile which confuses shepherd diagnostics.
git -C "$WORKTREE_PATH" checkout -- Cargo.lock 2>/dev/null || true
