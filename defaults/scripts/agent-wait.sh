#!/bin/bash
# agent-wait.sh - Wait for a tmux Claude agent to finish its task
#
# Detects when a Claude agent in a tmux session has completed its work
# by inspecting the process tree. When Claude finishes, the shell becomes
# idle (no claude child process). This script polls until that happens.
#
# Exit codes:
#   0 - Agent completed (shell is idle, no claude process)
#   1 - Timeout reached
#   2 - Session not found
#
# Usage:
#   agent-wait.sh <name> [--timeout <seconds>] [--poll-interval <seconds>] [--json]
#
# Examples:
#   agent-wait.sh builder-issue-42 --timeout 1800
#   agent-wait.sh shepherd-1 --poll-interval 10 --json

set -euo pipefail

# Configuration
TMUX_SOCKET="loom"
SESSION_PREFIX="loom-"
DEFAULT_TIMEOUT=3600
DEFAULT_POLL_INTERVAL=5

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$(date '+%H:%M:%S')]${NC} $*" >&2; }
log_success() { echo -e "${GREEN}[$(date '+%H:%M:%S')] ✓${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[$(date '+%H:%M:%S')] ⚠${NC} $*" >&2; }
log_error() { echo -e "${RED}[$(date '+%H:%M:%S')] ✗${NC} $*" >&2; }

show_help() {
    cat <<EOF
${BLUE}agent-wait.sh - Wait for a tmux Claude agent to complete${NC}

${YELLOW}USAGE:${NC}
    agent-wait.sh <name> [OPTIONS]

${YELLOW}OPTIONS:${NC}
    --timeout <seconds>        Maximum time to wait (default: $DEFAULT_TIMEOUT)
    --poll-interval <seconds>  Time between checks (default: $DEFAULT_POLL_INTERVAL)
    --json                     Output result as JSON
    --help                     Show this help message

${YELLOW}EXIT CODES:${NC}
    0  Agent completed (no claude process running in session)
    1  Timeout reached
    2  Session not found

${YELLOW}EXAMPLES:${NC}
    agent-wait.sh builder-issue-42 --timeout 1800
    agent-wait.sh shepherd-1 --poll-interval 10 --json

${YELLOW}HOW IT WORKS:${NC}
    Finds the tmux session's shell PID, then checks if any claude
    process exists in the process tree under that shell. When claude
    exits, the shell is idle and the agent is considered complete.

EOF
}

# Get the shell PID for a tmux session
get_session_shell_pid() {
    local session_name="$1"
    tmux -L "$TMUX_SOCKET" list-panes -t "$session_name" -F '#{pane_pid}' 2>/dev/null | head -1
}

# Check if claude is running under a given PID
claude_is_running() {
    local shell_pid="$1"

    # Check if any child process of the shell is claude
    # Use pgrep to find claude processes with the shell as parent
    # We check recursively since claude may be wrapped
    if pgrep -P "$shell_pid" -f "claude" >/dev/null 2>&1; then
        return 0
    fi

    # Also check grandchildren (claude-wrapper.sh -> claude)
    local children
    children=$(pgrep -P "$shell_pid" 2>/dev/null || true)
    for child in $children; do
        if pgrep -P "$child" -f "claude" >/dev/null 2>&1; then
            return 0
        fi
    done

    return 1
}

# Check if session exists
session_exists() {
    local session_name="$1"
    tmux -L "$TMUX_SOCKET" has-session -t "$session_name" 2>/dev/null
}

main() {
    local name=""
    local timeout="$DEFAULT_TIMEOUT"
    local poll_interval="$DEFAULT_POLL_INTERVAL"
    local json_output=false

    # Parse arguments
    if [[ $# -lt 1 ]]; then
        show_help
        exit 2
    fi

    # First positional arg is the name
    name="$1"
    shift

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --timeout)
                timeout="$2"
                shift 2
                ;;
            --poll-interval)
                poll_interval="$2"
                shift 2
                ;;
            --json)
                json_output=true
                shift
                ;;
            --help|-h)
                show_help
                exit 0
                ;;
            *)
                log_error "Unknown argument: $1"
                exit 2
                ;;
        esac
    done

    local session_name="${SESSION_PREFIX}${name}"

    # Check session exists
    if ! session_exists "$session_name"; then
        if [[ "$json_output" == "true" ]]; then
            echo "{\"status\":\"not_found\",\"name\":\"$name\",\"session\":\"$session_name\"}"
        else
            log_error "Session not found: $session_name"
        fi
        exit 2
    fi

    # Get the shell PID
    local shell_pid
    shell_pid=$(get_session_shell_pid "$session_name")
    if [[ -z "$shell_pid" ]]; then
        if [[ "$json_output" == "true" ]]; then
            echo "{\"status\":\"error\",\"name\":\"$name\",\"error\":\"could not find shell PID\"}"
        else
            log_error "Could not find shell PID for session: $session_name"
        fi
        exit 2
    fi

    log_info "Waiting for agent '$name' to complete (timeout: ${timeout}s, poll: ${poll_interval}s)"
    log_info "Session: $session_name, Shell PID: $shell_pid"

    local elapsed=0
    local start_time
    start_time=$(date +%s)

    while true; do
        # Check if session still exists (may have been destroyed)
        if ! session_exists "$session_name"; then
            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"completed\",\"name\":\"$name\",\"reason\":\"session_destroyed\",\"elapsed\":$elapsed}"
            else
                log_success "Agent '$name' completed (session destroyed after ${elapsed}s)"
            fi
            exit 0
        fi

        # Re-fetch shell PID in case pane was recreated
        shell_pid=$(get_session_shell_pid "$session_name")
        if [[ -z "$shell_pid" ]]; then
            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"completed\",\"name\":\"$name\",\"reason\":\"no_shell\",\"elapsed\":$elapsed}"
            else
                log_success "Agent '$name' completed (no shell process after ${elapsed}s)"
            fi
            exit 0
        fi

        # Check if claude is still running
        if ! claude_is_running "$shell_pid"; then
            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"completed\",\"name\":\"$name\",\"reason\":\"claude_exited\",\"elapsed\":$elapsed}"
            else
                log_success "Agent '$name' completed (claude exited after ${elapsed}s)"
            fi
            exit 0
        fi

        # Check timeout
        elapsed=$(( $(date +%s) - start_time ))
        if [[ "$elapsed" -ge "$timeout" ]]; then
            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"timeout\",\"name\":\"$name\",\"elapsed\":$elapsed,\"timeout\":$timeout}"
            else
                log_warn "Timeout waiting for agent '$name' after ${elapsed}s"
            fi
            exit 1
        fi

        sleep "$poll_interval"
    done
}

main "$@"
