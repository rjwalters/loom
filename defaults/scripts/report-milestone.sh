#!/bin/bash

# report-milestone.sh - Thin stub that delegates to Python loom-milestone CLI
#
# This script exists for bash callers (e.g., validate-phase.sh) that need
# to report milestones from shell scripts. Python callers should import
# loom_tools.milestones.report_milestone() directly instead.
#
# Usage:
#   report-milestone.sh <event> [options]
#
# See `loom-milestone --help` for full usage.

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-milestone >/dev/null 2>&1; then
    exec loom-milestone "$@"
else
    exec python3 -m loom_tools.milestones "$@"
fi
