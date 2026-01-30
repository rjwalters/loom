#!/usr/bin/env bash
# Wrapper to invoke Python daemon-loop with proper environment
#
# This script provides the entry point for the daemon loop, routing to
# the Python-based daemon implementation (loom-tools package).
#
# Benefits over direct shell script:
#   - Handles PYTHONPATH setup for both pip-installed and source installs
#   - Maps CLI flags for user-facing parity (--merge/-m -> --force/-f)
#   - Graceful fallback to shell script if Python unavailable
#   - Single change point for daemon command routing
#
# Usage:
#   ./.loom/scripts/daemon-loop.sh [options]
#
# Options (user-facing):
#   --merge, -m     Enable merge mode for aggressive autonomous development
#   --debug, -d     Enable debug mode for verbose subagent troubleshooting
#   --status        Check if daemon loop is running
#   --health        Show daemon health status and exit
#
# Environment Variables:
#   LOOM_POLL_INTERVAL - Seconds between iterations (default: 120)
#   LOOM_ITERATION_TIMEOUT - Max seconds per iteration (default: 300)
#   LOOM_MAX_BACKOFF - Maximum backoff interval in seconds (default: 1800)
#   LOOM_BACKOFF_MULTIPLIER - Backoff multiplier on failure (default: 2)
#   LOOM_BACKOFF_THRESHOLD - Failures before backoff kicks in (default: 3)
#
# See loom.md for full documentation.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOOM_TOOLS="$REPO_ROOT/loom-tools"

# Map --merge/-m to --force/-f for CLI parity
# The Python CLI uses --force internally, but users expect --merge per documentation
args=()
for arg in "$@"; do
    case "$arg" in
        --merge|-m)
            args+=("--force")
            ;;
        *)
            args+=("$arg")
            ;;
    esac
done

# Try Python implementation first
# Priority order:
#   1. Virtual environment in loom-tools (development setup)
#   2. System-installed loom-daemon-loop (pip install)
#   3. Fallback to shell script (transition period)

if [[ -x "$LOOM_TOOLS/.venv/bin/loom-daemon-loop" ]]; then
    # Development setup: use venv directly
    exec "$LOOM_TOOLS/.venv/bin/loom-daemon-loop" "${args[@]}"
elif command -v loom-daemon-loop &>/dev/null; then
    # System-installed
    exec loom-daemon-loop "${args[@]}"
else
    # Fallback to shell script (deprecated)
    # Note: Shell script uses --merge/-m directly, so pass original args
    echo "[WARN] Python daemon-loop not available, falling back to shell script (deprecated)" >&2
    exec "$SCRIPT_DIR/deprecated/daemon-loop.sh" "$@"
fi
