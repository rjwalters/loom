#!/bin/bash
# daemon-cleanup.sh - Event-driven cleanup for the Loom daemon
#
# This is a thin stub that delegates to the Python implementation.
# See loom-tools/src/loom_tools/daemon_cleanup.py for the full implementation.
#
# Usage:
#   daemon-cleanup.sh shepherd-complete <issue>  # Cleanup after shepherd finishes
#   daemon-cleanup.sh daemon-startup             # Cleanup stale artifacts
#   daemon-cleanup.sh daemon-shutdown            # Archive logs and cleanup
#   daemon-cleanup.sh periodic                   # Conservative periodic cleanup
#   daemon-cleanup.sh prune-sessions             # Prune old session archives
#   daemon-cleanup.sh <event> --dry-run          # Preview cleanup
#   daemon-cleanup.sh --help                     # Show help

set -euo pipefail

# Try the installed Python entry point first, fall back to module invocation
if command -v loom-daemon-cleanup >/dev/null 2>&1; then
    exec loom-daemon-cleanup "$@"
else
    exec python3 -m loom_tools.daemon_cleanup "$@"
fi
