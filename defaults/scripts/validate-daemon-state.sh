#!/usr/bin/env bash
# validate-daemon-state.sh - Thin stub delegating to Python implementation
#
# See loom-tools/src/loom_tools/validate_state.py for the full implementation.

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-validate-state >/dev/null 2>&1; then
    exec loom-validate-state "$@"
else
    exec python3 -m loom_tools.validate_state "$@"
fi
