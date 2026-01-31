#!/usr/bin/env bash
# Backwards-compatible wrapper for loom-clean
# Routes to loom-clean (Python) from loom-tools package.
# Use "loom-clean" directly if available in PATH.

set -euo pipefail

# Priority order:
#   1. loom-clean in PATH (pip install -e ./loom-tools)
#   2. Python module invocation (fallback)
if command -v loom-clean &>/dev/null; then
  exec loom-clean "$@"
elif python3 -c "import loom_tools.clean" &>/dev/null 2>&1; then
  exec python3 -m loom_tools.clean "$@"
else
  echo "Error: loom-clean not available. Install loom-tools:" >&2
  echo "  pip install -e ./loom-tools" >&2
  echo "" >&2
  echo "Or with uv:" >&2
  echo "  uv pip install -e ./loom-tools" >&2
  exit 1
fi
