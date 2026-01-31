#!/bin/bash
# recover-orphaned-shepherds.sh - Detect and recover orphaned shepherd state
#
# This is a thin stub that delegates to the Python implementation.
# See loom-tools/src/loom_tools/orphan_recovery.py for the full implementation.
#
# Usage:
#   recover-orphaned-shepherds.sh              # Dry-run: show what would be recovered
#   recover-orphaned-shepherds.sh --recover    # Actually recover orphaned state
#   recover-orphaned-shepherds.sh --json       # Output JSON for programmatic use
#   recover-orphaned-shepherds.sh --help       # Show help

set -euo pipefail

exec loom-recover-orphans "$@"
