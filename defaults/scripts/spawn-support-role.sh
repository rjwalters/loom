#!/usr/bin/env bash
# spawn-support-role.sh - Spawn support roles with interval checking and validation
#
# This script handles spawning support roles (champion, judge, doctor, guide,
# auditor, architect, hermit) with proper cooldown/interval checking.
#
# IMPORTANT: This script does NOT spawn the actual Task subagent. It validates
# that spawning is appropriate and outputs a spawn command that the calling
# LLM context must execute.
#
# Usage:
#   ./spawn-support-role.sh --role <name> [--demand|--interval]
#   ./spawn-support-role.sh --role champion --demand
#   ./spawn-support-role.sh --role judge --interval
#
# Exit codes:
#   0 - Success (role ready to spawn)
#   1 - Role already running
#   2 - Cooldown not elapsed
#   3 - Invalid role name
#   4 - Invalid arguments
#
# Output (JSON):
#   {
#     "success": true,
#     "role": "champion",
#     "mode": "demand",
#     "spawn_command": "/champion",
#     "task_id_pattern": "^[a-f0-9]{7}$"
#   }

set -euo pipefail

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Role intervals (in seconds)
# Using simple variables for bash 3.x compatibility (macOS default)
INTERVAL_guide="${LOOM_GUIDE_INTERVAL:-900}"
INTERVAL_champion="${LOOM_CHAMPION_INTERVAL:-600}"
INTERVAL_doctor="${LOOM_DOCTOR_INTERVAL:-300}"
INTERVAL_auditor="${LOOM_AUDITOR_INTERVAL:-600}"
INTERVAL_judge="${LOOM_JUDGE_INTERVAL:-300}"
INTERVAL_architect="${LOOM_ARCHITECT_COOLDOWN:-1800}"
INTERVAL_hermit="${LOOM_HERMIT_COOLDOWN:-1800}"

# All valid roles
VALID_ROLES="guide|champion|doctor|auditor|judge|architect|hermit"

# Get interval for a role (bash 3.x compatible)
get_role_interval_value() {
    local role="$1"
    case "$role" in
        guide) echo "$INTERVAL_guide" ;;
        champion) echo "$INTERVAL_champion" ;;
        doctor) echo "$INTERVAL_doctor" ;;
        auditor) echo "$INTERVAL_auditor" ;;
        judge) echo "$INTERVAL_judge" ;;
        architect) echo "$INTERVAL_architect" ;;
        hermit) echo "$INTERVAL_hermit" ;;
        *) echo "300" ;;  # Default 5 minutes
    esac
}

# Find the repository root
find_repo_root() {
    local dir="$PWD"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.git" ]] || [[ -f "$dir/.git" ]]; then
            echo "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    echo "Error: Not in a git repository" >&2
    return 1
}

REPO_ROOT=$(find_repo_root)
STATE_FILE="$REPO_ROOT/.loom/daemon-state.json"

# Parse arguments
ROLE=""
MODE="interval"  # Default to interval-based
DRY_RUN=false
FORCE=false
VERBOSE=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --role|-r)
            ROLE="$2"
            shift 2
            ;;
        --demand|-d)
            MODE="demand"
            shift
            ;;
        --interval|-i)
            MODE="interval"
            shift
            ;;
        --force)
            FORCE=true
            shift
            ;;
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        --help|-h)
            cat <<EOF
Usage: $0 --role <name> [OPTIONS]

Spawn support role with interval/cooldown checking.

Required:
  --role, -r <name>    Role name (guide, champion, doctor, auditor, judge, architect, hermit)

Options:
  --demand, -d         Spawn on-demand (skip interval check)
  --interval, -i       Spawn if interval elapsed (default)
  --force              Spawn even if already running
  --dry-run            Validate without spawning
  --verbose            Show detailed progress
  --help               Show this help message

Role intervals (configurable via environment):
  guide:     ${LOOM_GUIDE_INTERVAL:-900}s (LOOM_GUIDE_INTERVAL)
  champion:  ${LOOM_CHAMPION_INTERVAL:-600}s (LOOM_CHAMPION_INTERVAL)
  doctor:    ${LOOM_DOCTOR_INTERVAL:-300}s (LOOM_DOCTOR_INTERVAL)
  auditor:   ${LOOM_AUDITOR_INTERVAL:-600}s (LOOM_AUDITOR_INTERVAL)
  judge:     ${LOOM_JUDGE_INTERVAL:-300}s (LOOM_JUDGE_INTERVAL)
  architect: ${LOOM_ARCHITECT_COOLDOWN:-1800}s (LOOM_ARCHITECT_COOLDOWN)
  hermit:    ${LOOM_HERMIT_COOLDOWN:-1800}s (LOOM_HERMIT_COOLDOWN)

Exit codes:
  0 - Success (role ready to spawn)
  1 - Role already running
  2 - Cooldown not elapsed
  3 - Invalid role name
  4 - Invalid arguments

Examples:
  $0 --role champion --demand     # Spawn champion on-demand
  $0 --role judge --interval      # Spawn judge if interval elapsed
  $0 --role architect             # Check cooldown before spawning
  $0 --role guide --dry-run       # Check if would spawn
EOF
            exit 0
            ;;
        *)
            echo '{"success": false, "error": "unknown_option", "option": "'"$1"'"}'
            exit 4
            ;;
    esac
done

# Validate required arguments
if [[ -z "$ROLE" ]]; then
    echo '{"success": false, "error": "missing_role"}'
    exit 4
fi

# Validate role name
if ! [[ "$ROLE" =~ ^($VALID_ROLES)$ ]]; then
    echo '{"success": false, "error": "invalid_role", "role": "'"$ROLE"'", "valid_roles": "'"$VALID_ROLES"'"}'
    exit 3
fi

log_verbose() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo "[DEBUG] $*" >&2
    fi
}

# Get current timestamp
get_timestamp() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

# Get epoch from ISO timestamp
iso_to_epoch() {
    local iso="$1"
    if [[ "$(uname)" == "Darwin" ]]; then
        date -j -f "%Y-%m-%dT%H:%M:%SZ" "$iso" "+%s" 2>/dev/null || echo "0"
    else
        date -d "$iso" "+%s" 2>/dev/null || echo "0"
    fi
}

NOW_EPOCH=$(date +%s)

# Wrapper function for getting role interval
get_role_interval() {
    local role="$1"
    get_role_interval_value "$role"
}

COOLDOWN=$(get_role_interval "$ROLE")
log_verbose "Role: $ROLE, Cooldown: ${COOLDOWN}s, Mode: $MODE"

# Load state if it exists
if [[ -f "$STATE_FILE" ]]; then
    STATE=$(cat "$STATE_FILE")
else
    STATE='{}'
fi

# Check if role is already running
log_verbose "Checking if role is already running..."
ROLE_STATUS=$(echo "$STATE" | jq -r ".support_roles[\"$ROLE\"].status // \"idle\"")
ROLE_TASK_ID=$(echo "$STATE" | jq -r ".support_roles[\"$ROLE\"].task_id // empty")

if [[ "$ROLE_STATUS" == "running" && -n "$ROLE_TASK_ID" && "$FORCE" != "true" ]]; then
    # Role is already running - check if we should skip
    if [[ "$MODE" == "interval" ]]; then
        echo '{
  "success": false,
  "error": "role_already_running",
  "role": "'"$ROLE"'",
  "task_id": "'"$ROLE_TASK_ID"'",
  "message": "Role is already running. Use --force to spawn anyway."
}'
        exit 1
    fi
    # Demand mode - continue even if running
    log_verbose "Role running but demand mode - continuing"
fi

# Check cooldown for interval-based spawning
if [[ "$MODE" == "interval" && "$FORCE" != "true" ]]; then
    log_verbose "Checking cooldown..."

    # Get last completed time
    LAST_COMPLETED=$(echo "$STATE" | jq -r ".support_roles[\"$ROLE\"].last_completed // empty")

    # For work generation roles, also check the specific trigger timestamp
    if [[ "$ROLE" == "architect" ]]; then
        LAST_TRIGGER=$(echo "$STATE" | jq -r '.last_architect_trigger // empty')
        if [[ -n "$LAST_TRIGGER" ]]; then
            LAST_COMPLETED="$LAST_TRIGGER"
        fi
    elif [[ "$ROLE" == "hermit" ]]; then
        LAST_TRIGGER=$(echo "$STATE" | jq -r '.last_hermit_trigger // empty')
        if [[ -n "$LAST_TRIGGER" ]]; then
            LAST_COMPLETED="$LAST_TRIGGER"
        fi
    fi

    if [[ -n "$LAST_COMPLETED" ]]; then
        LAST_EPOCH=$(iso_to_epoch "$LAST_COMPLETED")
        ELAPSED=$((NOW_EPOCH - LAST_EPOCH))
        log_verbose "Last completed: $LAST_COMPLETED, Elapsed: ${ELAPSED}s, Cooldown: ${COOLDOWN}s"

        if [[ $ELAPSED -lt $COOLDOWN ]]; then
            REMAINING=$((COOLDOWN - ELAPSED))
            echo '{
  "success": false,
  "error": "cooldown_not_elapsed",
  "role": "'"$ROLE"'",
  "elapsed_seconds": '"$ELAPSED"',
  "cooldown_seconds": '"$COOLDOWN"',
  "remaining_seconds": '"$REMAINING"',
  "message": "Cooldown not elapsed. Wait '"$REMAINING"'s or use --demand."
}'
            exit 2
        fi
    fi
fi

# Dry run exits here
if [[ "$DRY_RUN" == "true" ]]; then
    echo '{
  "success": true,
  "dry_run": true,
  "role": "'"$ROLE"'",
  "mode": "'"$MODE"'",
  "cooldown_seconds": '"$COOLDOWN"',
  "would_spawn": true,
  "spawn_command": "/'"$ROLE"'"
}'
    exit 0
fi

# Generate spawn command
SPAWN_COMMAND="/$ROLE"

# Output success JSON
echo '{
  "success": true,
  "role": "'"$ROLE"'",
  "mode": "'"$MODE"'",
  "cooldown_seconds": '"$COOLDOWN"',
  "spawn_command": "'"$SPAWN_COMMAND"'",
  "task_id_pattern": "^[a-f0-9]{7}$",
  "timestamp": "'"$(get_timestamp)"'",
  "instructions": {
    "step1": "Execute spawn_command via Task(prompt=spawn_command, run_in_background=True)",
    "step2": "Validate returned task_id matches task_id_pattern",
    "step3": "Record in daemon-state.json: support_roles[role] = {status: running, task_id: ..., started_at: ...}",
    "step4": "For architect/hermit, also update last_<role>_trigger timestamp"
  }
}'

exit 0
