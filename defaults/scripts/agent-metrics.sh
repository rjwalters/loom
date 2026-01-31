#!/bin/bash

# agent-metrics.sh - Thin stub delegating to loom-agent-metrics (Python)
#
# This script preserves the CLI interface for backwards compatibility.
# The MCP tool (mcp-loom) shells out to this script, so the interface
# must remain stable.
#
# Usage:
#   agent-metrics.sh [--role ROLE] [--period PERIOD] [--format FORMAT]
#   agent-metrics.sh summary
#   agent-metrics.sh effectiveness [--role ROLE]
#   agent-metrics.sh costs [--issue NUMBER]
#   agent-metrics.sh velocity
#   agent-metrics.sh --help

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-agent-metrics >/dev/null 2>&1; then
    exec loom-agent-metrics "$@"
else
    exec python3 -m loom_tools.agent_metrics "$@"
fi
