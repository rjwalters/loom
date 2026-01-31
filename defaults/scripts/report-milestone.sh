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

exec loom-milestone "$@"
