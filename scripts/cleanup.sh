#!/bin/bash
# cleanup.sh - Log archival for Loom
#
# Thin stub that delegates to the Python implementation in
# loom-tools/src/loom_tools/cleanup.py (entry point: loom-cleanup).
#
# History: this script was previously named daemon-cleanup.sh and dispatched
# event-driven cleanup for the Loom daemon (shepherd-complete, daemon-startup,
# daemon-shutdown, periodic, prune-sessions).  Those events are removed in
# #3396 (Phase 3.1.7 of #3372) -- session rotation goes away with the daemon
# brain in Phase 3.2.  Only log archival survives.
#
# Usage:
#   cleanup.sh logs                          # archive task outputs + prune
#   cleanup.sh logs --dry-run                # preview
#   cleanup.sh logs --prune-only             # skip archival, only prune
#   cleanup.sh logs --retention-days N       # override retention window
#   cleanup.sh --help                        # show help

set -euo pipefail

exec loom-cleanup "$@"
