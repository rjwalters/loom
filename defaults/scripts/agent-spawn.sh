#!/bin/bash
# agent-spawn.sh - Thin stub that delegates to the Python loom-agent-spawn CLI.
#
# This script preserves backward compatibility for callers that invoke
# agent-spawn.sh directly.  All logic now lives in:
#   loom-tools/src/loom_tools/agent_spawn.py
#
# Usage is unchanged — all flags are forwarded as-is:
#   agent-spawn.sh --role <role> --name <name> [--args "<args>"] [--worktree <path>]
#   agent-spawn.sh --check <name>
#   agent-spawn.sh --list
#   agent-spawn.sh --help

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-agent-spawn >/dev/null 2>&1; then
    exec loom-agent-spawn "$@"
else
    exec python3 -m loom_tools.agent_spawn "$@"
fi
