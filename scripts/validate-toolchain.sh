#!/bin/bash
# validate-toolchain.sh - Validate the Loom command toolchain is available
#
# Validates that the commands the daemon needs are installed and accessible
# before it enters its main loop. They are all native `loom-daemon`
# subcommands — the Python `loom-tools` package this script was written against
# was retired in epic #4081 Phase 4 (#4557).
#
# See defaults/scripts/validate-toolchain.sh for the full implementation.
#
# Usage:
#   validate-toolchain.sh           # Validate all commands
#   validate-toolchain.sh --quick   # Only validate critical commands
#   validate-toolchain.sh --json    # JSON output for automation
#   validate-toolchain.sh --help    # Show help

set -euo pipefail

# Use the defaults script directly (this is the Loom source repo)
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec "${REPO_ROOT}/defaults/scripts/validate-toolchain.sh" "$@"
