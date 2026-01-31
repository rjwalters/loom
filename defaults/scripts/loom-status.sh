#!/bin/bash

# loom-status.sh - Read-only system status for Layer 3 observation
#
# This is a thin stub that delegates to the Python CLI (loom-status).
# The full implementation was ported from bash to Python in loom-tools.
#
# Usage:
#   loom-status.sh              - Display full system status
#   loom-status.sh --json       - Output status as JSON
#   loom-status.sh --help       - Show help

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-status >/dev/null 2>&1; then
    exec loom-status "$@"
else
    exec python3 -m loom_tools.status "$@"
fi
