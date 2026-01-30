#!/bin/bash
# constants.sh - Shared constants for Loom scripts
#
# This file provides shared patterns and constants used across multiple
# Loom scripts to ensure consistency and avoid drift.
#
# Usage:
#   # At the top of your script, after determining SCRIPT_DIR:
#   source "${SCRIPT_DIR}/lib/constants.sh"

# ==============================================================================
# Processing Detection Patterns
# ==============================================================================
#
# Pattern for detecting Claude Code activity in a tmux pane.
# Used to determine if Claude is actively processing a command.
#
# This includes:
# - Braille spinner characters (Claude thinking indicator)
# - Status text indicators (Beaming, Loading, etc.)
# - Progress indicators (●, ✓, spinning characters)
# - Activity keywords (thinking, streaming, Wandering)
#
# Used by:
# - agent-spawn.sh: Verify Claude started processing after spawn
# - agent-wait-bg.sh: Detect stuck-at-prompt condition
#
PROCESSING_INDICATORS='⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Beaming|Loading|● |✓ |◐|◓|◑|◒|thinking|streaming|Wandering'

# ==============================================================================
# Future shared constants can be added below
# ==============================================================================
