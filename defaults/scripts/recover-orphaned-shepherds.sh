#!/bin/bash
# recover-orphaned-shepherds.sh - Detect and recover orphaned shepherd state
#
# This is a thin stub that delegates to the Python implementation.
# See loom-tools/src/loom_tools/orphan_recovery.py for the full implementation.
#
# Usage:
#   recover-orphaned-shepherds.sh              # Dry-run: show what would be recovered
#   recover-orphaned-shepherds.sh --recover    # Actually recover orphaned state
#   recover-orphaned-shepherds.sh --json       # Output JSON for programmatic use
#   recover-orphaned-shepherds.sh --help       # Show help

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-recover-orphans >/dev/null 2>&1; then
    exec loom-recover-orphans "$@"
else
    exec python3 -m loom_tools.orphan_recovery "$@"
fi
