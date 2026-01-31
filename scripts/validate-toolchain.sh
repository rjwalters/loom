#!/bin/bash
# validate-toolchain.sh - Validate loom-tools commands are available
#
# Validates that essential loom-tools commands are installed and accessible
# before the daemon enters its main loop.
#
# See defaults/scripts/validate-toolchain.sh for the full implementation
# with fallback logic for installations.
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
