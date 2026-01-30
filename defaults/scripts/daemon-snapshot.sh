#!/bin/bash
# daemon-snapshot.sh - Consolidated daemon state snapshot
#
# Usage:
#   daemon-snapshot.sh                # Output JSON snapshot
#   daemon-snapshot.sh --pretty       # Pretty-printed JSON
#   daemon-snapshot.sh --help         # Show help
#
# This script delegates to the Python implementation in loom-tools.
# The Python version produces identical JSON output to the former
# shell version, using typed models and concurrent.futures for
# parallel GitHub API queries.
#
# Environment variables (LOOM_*) are passed through to Python.

set -euo pipefail

# Find the repository root (works from worktrees)
find_repo_root() {
    local dir="$PWD"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.git" ]] || [[ -f "$dir/.git" ]]; then
            # Handle worktree .git files
            if [[ -f "$dir/.git" ]]; then
                local gitdir
                gitdir=$(sed -n 's/^gitdir: //p' "$dir/.git")
                if [[ -n "$gitdir" ]]; then
                    local resolved
                    resolved=$(cd "$dir" && cd "$gitdir" && pwd)
                    local p="$resolved"
                    while [[ "$(basename "$p")" != ".git" ]] && [[ "$p" != "/" ]]; do
                        p="$(dirname "$p")"
                    done
                    if [[ "$(basename "$p")" == ".git" ]]; then
                        echo "$(dirname "$p")"
                        return 0
                    fi
                fi
            fi
            echo "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    echo "Error: Not in a git repository" >&2
    return 1
}

REPO_ROOT=$(find_repo_root)
LOOM_TOOLS="$REPO_ROOT/loom-tools/src"

# Verify python3 is available
if ! command -v python3 >/dev/null 2>&1; then
    echo "Error: python3 is required but not found in PATH" >&2
    exit 1
fi

# Delegate to Python implementation
exec env PYTHONPATH="$LOOM_TOOLS${PYTHONPATH:+:$PYTHONPATH}" \
    python3 -m loom_tools.snapshot "$@"
