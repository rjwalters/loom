#!/bin/bash

# health-check.sh - Proactive health monitoring and alerting for Loom daemon
#
# This is a thin stub that delegates to the Python CLI (loom-health-monitor).
# The full implementation was ported from bash to Python in loom-tools.
#
# Usage:
#   health-check.sh                    # Display health summary
#   health-check.sh --json             # Output health status as JSON
#   health-check.sh --collect          # Collect and store health metrics
#   health-check.sh --alerts           # Show current alerts
#   health-check.sh --acknowledge <id> # Acknowledge an alert
#   health-check.sh --clear-alerts     # Clear all alerts
#   health-check.sh --history [hours]  # Show metric history

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-health-monitor >/dev/null 2>&1; then
    exec loom-health-monitor "$@"
else
    exec python3 -m loom_tools.health_monitor "$@"
fi
