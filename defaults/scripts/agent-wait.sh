#!/bin/bash
# agent-wait.sh - Wait for a tmux Claude agent to finish its task
#
# Thin stub that delegates to the Python loom-agent-wait CLI.
# The Python implementation provides identical behavior with better
# error handling, testability, and structured output.
#
# Exit codes:
#   0 - Agent completed (shell is idle, no claude process)
#   1 - Timeout reached
#   2 - Session not found
#
# Usage:
#   agent-wait.sh <name> [--timeout <seconds>] [--poll-interval <seconds>] [--json]

set -euo pipefail

# Check if Python CLI is available
if command -v loom-agent-wait >/dev/null 2>&1; then
    exec loom-agent-wait "$@"
fi

# Fallback: try running as a Python module
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ -d "${REPO_ROOT}/loom-tools/src/loom_tools" ]]; then
    exec python3 -m loom_tools.agent_wait "$@"
fi

echo "ERROR: loom-agent-wait not found. Install loom-tools: pip install -e loom-tools/" >&2
exit 2
