#!/usr/bin/env bash
# Loom Cleanup - Wrapper script for repository maintenance
# Routes to loom-clean (Python replacement from loom-tools).
# In target repositories, use loom-clean directly.

set -euo pipefail

# Determine script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_TOOLS="$SCRIPT_DIR/loom-tools"

# Priority order:
#   1. Virtual environment in loom-tools (development setup)
#   2. System-installed loom-clean (pip install)
if [[ -x "$LOOM_TOOLS/.venv/bin/loom-clean" ]]; then
  exec "$LOOM_TOOLS/.venv/bin/loom-clean" "$@"
elif command -v loom-clean &>/dev/null; then
  exec loom-clean "$@"
else
  echo "Error: loom-clean not available. Install loom-tools:" >&2
  echo "  cd loom-tools && pip install -e ." >&2
  exit 1
fi
