#!/usr/bin/env bash
# spawn-shepherd-direct.sh - Atomic shepherd spawning with validation and rollback
#
# This script performs atomic shepherd spawning: claim -> worktree -> spawn.
# If any step fails, it rolls back previous steps to maintain consistency.
#
# IMPORTANT: This script does NOT spawn the actual Task subagent. It prepares
# everything and outputs a spawn command that the calling LLM context must
# execute. This is because bash cannot invoke Claude Task tools directly.
#
# Usage:
#   ./spawn-shepherd-direct.sh --issue <N> [--force-pr|--force-merge]
#   ./spawn-shepherd-direct.sh --issue 42 --force-pr
#   ./spawn-shepherd-direct.sh --dry-run --issue 42
#
# Exit codes:
#   0 - Success (shepherd ready to spawn)
#   1 - Issue not ready (wrong labels)
#   2 - Claim failed
#   3 - Worktree creation failed
#   4 - Validation failed
#   5 - Invalid arguments
#
# Output (JSON):
#   {
#     "success": true,
#     "issue": 42,
#     "shepherd_slot": "shepherd-1",
#     "worktree_path": ".loom/worktrees/issue-42",
#     "branch": "feature/issue-42",
#     "spawn_command": "/shepherd 42 --force-pr",
#     "rollback_on_failure": ["unclaim:42", "remove_worktree:issue-42"]
#   }
#
# The caller MUST:
#   1. Execute the spawn_command via Task tool
#   2. If spawn fails, execute rollback_on_failure commands
#   3. Validate the returned task_id matches ^[a-f0-9]{7}$
#   4. Record the assignment in daemon-state.json

set -euo pipefail

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

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
WORKTREE_DIR="$REPO_ROOT/.loom/worktrees"

# Parse arguments
ISSUE=""
FORCE_MODE="--force-pr"  # Default to force-pr
DRY_RUN=false
JSON_OUTPUT=true  # Always JSON output for programmatic use
VERBOSE=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --issue|-i)
            ISSUE="$2"
            shift 2
            ;;
        --force-pr)
            FORCE_MODE="--force-pr"
            shift
            ;;
        --force-merge)
            FORCE_MODE="--force-merge"
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
Usage: $0 --issue <N> [OPTIONS]

Atomically prepare shepherd spawning with claim, worktree, and validation.

Required:
  --issue, -i <N>    Issue number to spawn shepherd for

Options:
  --force-pr         Stop at ready-to-merge (default)
  --force-merge      Auto-merge after Judge approval
  --dry-run          Validate without making changes
  --verbose          Show detailed progress
  --help             Show this help message

Exit codes:
  0 - Success (shepherd ready to spawn)
  1 - Issue not ready (wrong labels)
  2 - Claim failed
  3 - Worktree creation failed
  4 - Validation failed
  5 - Invalid arguments

Output:
  JSON object with spawn_command and rollback instructions

The caller must:
  1. Execute spawn_command via Task tool
  2. If spawn fails, execute rollback commands
  3. Validate task_id format (^[a-f0-9]{7}$)
  4. Record in daemon-state.json

Examples:
  $0 --issue 42                    # Prepare spawn with force-pr
  $0 --issue 42 --force-merge      # Prepare spawn with force-merge
  $0 --issue 42 --dry-run          # Validate only
EOF
            exit 0
            ;;
        *)
            echo '{"success": false, "error": "unknown_option", "option": "'"$1"'"}'
            exit 5
            ;;
    esac
done

# Validate required arguments
if [[ -z "$ISSUE" ]]; then
    echo '{"success": false, "error": "missing_issue_number"}'
    exit 5
fi

# Validate issue number is numeric
if ! [[ "$ISSUE" =~ ^[0-9]+$ ]]; then
    echo '{"success": false, "error": "invalid_issue_number", "issue": "'"$ISSUE"'"}'
    exit 5
fi

log_verbose() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo "[DEBUG] $*" >&2
    fi
}

# Track rollback actions
declare -a ROLLBACK_ACTIONS=()

# Function to perform rollback
do_rollback() {
    log_verbose "Performing rollback..."
    for action in "${ROLLBACK_ACTIONS[@]}"; do
        action_type=$(echo "$action" | cut -d: -f1)
        action_arg=$(echo "$action" | cut -d: -f2)

        case "$action_type" in
            unclaim)
                log_verbose "  Rolling back: unclaim issue #$action_arg"
                gh issue edit "$action_arg" --remove-label "loom:building" --add-label "loom:issue" 2>/dev/null || true
                ;;
            remove_worktree)
                log_verbose "  Rolling back: remove worktree $action_arg"
                git worktree remove "$WORKTREE_DIR/$action_arg" --force 2>/dev/null || true
                ;;
            delete_branch)
                log_verbose "  Rolling back: delete branch $action_arg"
                git branch -D "$action_arg" 2>/dev/null || true
                ;;
        esac
    done
}

# Trap for cleanup on error
cleanup_on_error() {
    local exit_code=$?
    if [[ $exit_code -ne 0 && ${#ROLLBACK_ACTIONS[@]} -gt 0 ]]; then
        do_rollback
    fi
}
trap cleanup_on_error EXIT

# Step 1: Validate issue state
log_verbose "Step 1: Validating issue #$ISSUE..."

ISSUE_DATA=$(gh issue view "$ISSUE" --json labels,state,title 2>/dev/null || echo '{"error": "not_found"}')

if echo "$ISSUE_DATA" | jq -e '.error' >/dev/null 2>&1; then
    echo '{"success": false, "error": "issue_not_found", "issue": '"$ISSUE"'}'
    exit 1
fi

ISSUE_STATE=$(echo "$ISSUE_DATA" | jq -r '.state')
ISSUE_LABELS=$(echo "$ISSUE_DATA" | jq -r '.labels[].name' | tr '\n' ',' | sed 's/,$//')
ISSUE_TITLE=$(echo "$ISSUE_DATA" | jq -r '.title')

# Check if issue is open
if [[ "$ISSUE_STATE" != "OPEN" ]]; then
    echo '{"success": false, "error": "issue_not_open", "issue": '"$ISSUE"', "state": "'"$ISSUE_STATE"'"}'
    exit 1
fi

# Check if issue has loom:issue label
if ! echo "$ISSUE_LABELS" | grep -q "loom:issue"; then
    echo '{"success": false, "error": "issue_not_ready", "issue": '"$ISSUE"', "labels": "'"$ISSUE_LABELS"'", "required": "loom:issue"}'
    exit 1
fi

# Check if issue is blocked
if echo "$ISSUE_LABELS" | grep -q "loom:blocked"; then
    echo '{"success": false, "error": "issue_blocked", "issue": '"$ISSUE"'}'
    exit 1
fi

log_verbose "  Issue #$ISSUE is ready: $ISSUE_TITLE"

# Dry run exits here
if [[ "$DRY_RUN" == "true" ]]; then
    echo '{
  "success": true,
  "dry_run": true,
  "issue": '"$ISSUE"',
  "title": "'"$(echo "$ISSUE_TITLE" | sed 's/"/\\"/g')"'",
  "force_mode": "'"$FORCE_MODE"'",
  "spawn_command": "/shepherd '"$ISSUE"' '"$FORCE_MODE"'",
  "would_create_worktree": "'"$WORKTREE_DIR/issue-$ISSUE"'"
}'
    exit 0
fi

# Step 2: Claim the issue
log_verbose "Step 2: Claiming issue #$ISSUE..."

if ! gh issue edit "$ISSUE" --remove-label "loom:issue" --add-label "loom:building" 2>/dev/null; then
    echo '{"success": false, "error": "claim_failed", "issue": '"$ISSUE"'}'
    exit 2
fi

ROLLBACK_ACTIONS+=("unclaim:$ISSUE")
log_verbose "  Claimed issue #$ISSUE (loom:issue -> loom:building)"

# Step 3: Create worktree
log_verbose "Step 3: Creating worktree..."

WORKTREE_PATH="$WORKTREE_DIR/issue-$ISSUE"
BRANCH_NAME="feature/issue-$ISSUE"

# Check if worktree already exists
if [[ -d "$WORKTREE_PATH" ]]; then
    log_verbose "  Worktree already exists: $WORKTREE_PATH"
else
    # Create worktree using the helper script if available
    if [[ -x "$REPO_ROOT/.loom/scripts/worktree.sh" ]]; then
        log_verbose "  Using worktree.sh helper..."
        # Worktree script expects to be run from repo root
        cd "$REPO_ROOT"
        if ! ./.loom/scripts/worktree.sh "$ISSUE" 2>/dev/null; then
            echo '{"success": false, "error": "worktree_creation_failed", "issue": '"$ISSUE"', "path": "'"$WORKTREE_PATH"'"}'
            exit 3
        fi
    else
        log_verbose "  Creating worktree manually..."
        # Ensure worktree directory exists
        mkdir -p "$WORKTREE_DIR"

        # Create branch and worktree
        if ! git worktree add "$WORKTREE_PATH" -b "$BRANCH_NAME" origin/main 2>/dev/null; then
            # Branch might already exist
            if ! git worktree add "$WORKTREE_PATH" "$BRANCH_NAME" 2>/dev/null; then
                echo '{"success": false, "error": "worktree_creation_failed", "issue": '"$ISSUE"', "path": "'"$WORKTREE_PATH"'"}'
                exit 3
            fi
        fi
        ROLLBACK_ACTIONS+=("delete_branch:$BRANCH_NAME")
    fi
fi

ROLLBACK_ACTIONS+=("remove_worktree:issue-$ISSUE")
log_verbose "  Worktree created: $WORKTREE_PATH"

# Step 4: Find available shepherd slot
log_verbose "Step 4: Finding shepherd slot..."

MAX_SHEPHERDS="${LOOM_MAX_SHEPHERDS:-3}"
SHEPHERD_SLOT=""

if [[ -f "$STATE_FILE" ]]; then
    for i in $(seq 1 "$MAX_SHEPHERDS"); do
        slot="shepherd-$i"
        status=$(jq -r ".shepherds[\"$slot\"].status // \"idle\"" "$STATE_FILE" 2>/dev/null || echo "idle")
        if [[ "$status" == "idle" || "$status" == "null" ]]; then
            SHEPHERD_SLOT="$slot"
            break
        fi
    done
fi

# If no slot found in state, use first available
if [[ -z "$SHEPHERD_SLOT" ]]; then
    SHEPHERD_SLOT="shepherd-1"
fi

log_verbose "  Using shepherd slot: $SHEPHERD_SLOT"

# Step 5: Generate spawn command and output
log_verbose "Step 5: Generating spawn command..."

SPAWN_COMMAND="/shepherd $ISSUE $FORCE_MODE"

# Build rollback actions JSON
rollback_json=$(printf '%s\n' "${ROLLBACK_ACTIONS[@]}" | jq -R . | jq -s .)

# Output success JSON
echo '{
  "success": true,
  "issue": '"$ISSUE"',
  "title": "'"$(echo "$ISSUE_TITLE" | sed 's/"/\\"/g')"'",
  "shepherd_slot": "'"$SHEPHERD_SLOT"'",
  "worktree_path": "'"$WORKTREE_PATH"'",
  "branch": "'"$BRANCH_NAME"'",
  "force_mode": "'"$FORCE_MODE"'",
  "spawn_command": "'"$SPAWN_COMMAND"'",
  "rollback_on_failure": '"$rollback_json"',
  "task_id_pattern": "^[a-f0-9]{7}$",
  "instructions": {
    "step1": "Execute spawn_command via Task(prompt=spawn_command, run_in_background=True)",
    "step2": "Validate returned task_id matches task_id_pattern",
    "step3": "If validation fails, execute rollback_on_failure commands",
    "step4": "Record in daemon-state.json: shepherds[shepherd_slot] = {...}"
  }
}'

# Clear the trap so we don't rollback on success
trap - EXIT

exit 0
