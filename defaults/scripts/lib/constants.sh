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
# IMPORTANT: Claude Code uses a large, changing library of status words
# (Beaming, Manifesting, Crafting, Wandering, etc.). Do NOT try to enumerate
# them all - they change between versions. Instead, we detect:
#
# 1. Spinner characters (stable across versions):
#    - Braille spinners: ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏
#    - Star spinners (v2.1.25+): ✻✶✳✢✽·
#    - Circle spinners: ◐◓◑◒
#
# 2. Tool/progress indicators:
#    - ⏺ (tool execution)
#    - ● (active indicator, followed by status text)
#
# Used by:
# - agent-spawn.sh: Verify Claude started processing after spawn
# - agent-wait-bg.sh: Detect stuck-at-prompt condition
#
# Note: The pattern uses alternation (|) for grep -E compatibility.
# Spinner characters are the most reliable indicators of activity.
#
# shellcheck disable=SC2034  # Variable is used by scripts that source this file
PROCESSING_INDICATORS='⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|✻|✶|✳|✢|✽|·|⏺|◐|◓|◑|◒|● '

# ==============================================================================
# Future shared constants can be added below
# ==============================================================================
