#!/usr/bin/env bash
# Safe worktree cleanup - WRAPPER (backwards compatibility)
#
# This script is a thin wrapper around loom-clean for backwards compatibility.
# The loom-clean Python tool handles all cleanup functionality with --safe mode.
#
# DEPRECATED: Use loom-clean --safe instead:
#   loom-clean --safe              # Equivalent to safe-worktree-cleanup.sh
#   loom-clean --safe --force      # Equivalent to safe-worktree-cleanup.sh --force
#
# What this wrapper does:
#   Maps safe-worktree-cleanup.sh arguments to loom-clean --safe equivalents
#   Always includes --safe and --worktrees-only

set -euo pipefail

# Find script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOOM_TOOLS="$PROJECT_ROOT/loom-tools"

# Map arguments - safe-worktree-cleanup.sh is worktrees-only with safe mode
ARGS=("--safe" "--worktrees-only")

for arg in "$@"; do
  case $arg in
    --dry-run)
      ARGS+=("--dry-run")
      ;;
    --force|-f)
      ARGS+=("--force")
      ;;
    --grace-period)
      ARGS+=("--grace-period")
      ;;
    --help|-h)
      cat <<EOF
Safe worktree cleanup - DEPRECATED WRAPPER

This script is a thin wrapper around loom-clean --safe for backwards compatibility.
Please use loom-clean --safe directly for more options.

Usage: ./scripts/safe-worktree-cleanup.sh [options]

Options:
  --dry-run           Show what would be cleaned without making changes
  -f, --force         Skip grace period and uncommitted changes check
  --grace-period N    Seconds to wait after PR merge (default: 600 = 10 min)
  -h, --help          Show this help message

Safety Features:
  - Only cleans worktrees with MERGED PRs (not just closed)
  - Checks for uncommitted changes before removal
  - Grace period after merge to avoid race conditions
  - Tracks cleanup state in daemon-state.json

Cleanup Criteria:
  A worktree is cleaned when ALL of the following are true:
  1. The associated issue is CLOSED
  2. The PR is MERGED (has mergedAt timestamp)
  3. Grace period has passed since merge
  4. No uncommitted changes exist (unless --force)

Equivalent loom-clean commands:
  ./scripts/safe-worktree-cleanup.sh             ->  loom-clean --safe --worktrees-only
  ./scripts/safe-worktree-cleanup.sh --force     ->  loom-clean --safe --worktrees-only --force
  ./scripts/safe-worktree-cleanup.sh --dry-run   ->  loom-clean --safe --worktrees-only --dry-run
EOF
      exit 0
      ;;
    *)
      ARGS+=("$arg")
      ;;
  esac
done

# Route to loom-clean (Python replacement)
if [[ -x "$LOOM_TOOLS/.venv/bin/loom-clean" ]]; then
  exec "$LOOM_TOOLS/.venv/bin/loom-clean" "${ARGS[@]}"
elif command -v loom-clean &>/dev/null; then
  exec loom-clean "${ARGS[@]}"
else
  echo "Error: loom-clean not available. Install loom-tools:" >&2
  echo "  cd loom-tools && pip install -e ." >&2
  exit 1
fi
