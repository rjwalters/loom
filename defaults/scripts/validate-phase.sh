#!/bin/bash

# validate-phase.sh - Thin stub that delegates to the Python implementation.
#
# Usage:
#   validate-phase.sh <phase> <issue-number> [options]
#
# This script is a compatibility shim.  The real implementation lives in
# loom-tools/src/loom_tools/validate_phase.py and is available as the
# ``loom-validate-phase`` CLI entry point.
#
# Exit codes:
#   0 - Contract satisfied (initially or after recovery)
#   1 - Contract failed, recovery failed or not possible
#   2 - Invalid arguments

set -euo pipefail

# Find the repository root (works from any subdirectory including worktrees)
find_repo_root() {
    local dir="$PWD"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.git" ]] || [[ -f "$dir/.git" ]]; then
            if [[ -f "$dir/.git" ]]; then
                local gitdir
                gitdir=$(cat "$dir/.git" | sed 's/^gitdir: //')
                if [[ "$gitdir" == /* ]]; then
                    dirname "$(dirname "$(dirname "$gitdir")")"
                else
                    dirname "$(dirname "$(dirname "$dir/$gitdir")")"
                fi
            else
                echo "$dir"
            fi
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    echo "$PWD"
}

REPO_ROOT="$(find_repo_root)"
LOOM_TOOLS="$REPO_ROOT/loom-tools"

# Try venv binary first, then PATH, then python3 -m fallback
if [[ -x "$LOOM_TOOLS/.venv/bin/loom-validate-phase" ]]; then
    exec "$LOOM_TOOLS/.venv/bin/loom-validate-phase" "$@"
elif command -v loom-validate-phase >/dev/null 2>&1; then
    exec loom-validate-phase "$@"
else
    exec python3 -m loom_tools.validate_phase "$@"
fi
