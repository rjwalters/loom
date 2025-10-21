#!/usr/bin/env bash
# Loom Cleanup - Wrapper script for repository maintenance
# This is a convenience wrapper for the Loom repository itself.
# In target repositories, use ./.loom/scripts/clean.sh directly.

set -euo pipefail

# Determine script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# If we're in the Loom repository (has defaults/scripts/clean.sh), use that
# Otherwise, use the installed version at .loom/scripts/clean.sh
if [[ -f "$SCRIPT_DIR/defaults/scripts/clean.sh" ]]; then
  # Loom repository - use the source version
  exec "$SCRIPT_DIR/defaults/scripts/clean.sh" "$@"
elif [[ -f "$SCRIPT_DIR/.loom/scripts/clean.sh" ]]; then
  # Target repository with Loom installed
  exec "$SCRIPT_DIR/.loom/scripts/clean.sh" "$@"
else
  echo "Error: Could not find clean.sh script"
  echo "  Expected at: defaults/scripts/clean.sh (Loom repo)"
  echo "           or: .loom/scripts/clean.sh (target repo)"
  exit 1
fi
