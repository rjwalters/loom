#!/bin/bash
# agent-wait-bg.sh - Wait for a tmux Claude agent with shutdown signal checking
#
# Wraps agent-wait.sh to run in the background while polling for shutdown signals.
# This allows shepherds to detect shutdown/abort requests during long waits
# instead of blocking until the phase completes.
#
# Exit codes:
#   0 - Agent completed (same as agent-wait.sh)
#   1 - Timeout reached (same as agent-wait.sh)
#   2 - Session not found (same as agent-wait.sh)
#   3 - Shutdown signal detected during wait
#
# Usage:
#   agent-wait-bg.sh <name> [--timeout <s>] [--poll-interval <s>] [--issue <N>] [--json]
#
# Examples:
#   agent-wait-bg.sh builder-issue-42 --timeout 1800 --issue 42
#   agent-wait-bg.sh shepherd-1 --poll-interval 10 --json

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# tmux configuration (must match agent-spawn.sh)
TMUX_SOCKET="loom"
SESSION_PREFIX="loom-"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$(date '+%H:%M:%S')]${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[$(date '+%H:%M:%S')] ⚠${NC} $*" >&2; }

# Default poll interval for signal checking
DEFAULT_SIGNAL_POLL=5

show_help() {
    cat <<EOF
${BLUE}agent-wait-bg.sh - Wait for agent with shutdown signal checking${NC}

${YELLOW}USAGE:${NC}
    agent-wait-bg.sh <name> [OPTIONS]

${YELLOW}OPTIONS:${NC}
    --timeout <seconds>        Maximum time to wait (default: 3600)
    --poll-interval <seconds>  Time between signal checks (default: $DEFAULT_SIGNAL_POLL)
    --issue <N>                Issue number for per-issue abort checking
    --json                     Output result as JSON
    --help                     Show this help message

${YELLOW}EXIT CODES:${NC}
    0  Agent completed
    1  Timeout reached
    2  Session not found
    3  Shutdown signal detected

${YELLOW}SIGNALS CHECKED:${NC}
    - .loom/stop-shepherds file (global shepherd shutdown)
    - loom:abort label on issue (per-issue abort, requires --issue)

${YELLOW}EXAMPLES:${NC}
    agent-wait-bg.sh builder-issue-42 --timeout 1800 --issue 42
    agent-wait-bg.sh curator-issue-10 --poll-interval 10 --json

EOF
}

# Check for shutdown signals
check_signals() {
    local issue="$1"

    # Check global shutdown signal
    if [ -f "${REPO_ROOT}/.loom/stop-shepherds" ]; then
        log_warn "Shutdown signal detected (stop-shepherds)"
        return 0
    fi

    # Check per-issue abort label
    if [ -n "$issue" ]; then
        local labels
        labels=$(gh issue view "$issue" --repo "$(gh repo view --json nameWithOwner --jq '.nameWithOwner')" --json labels --jq '.labels[].name' 2>/dev/null || true)
        if echo "$labels" | grep -q "loom:abort"; then
            log_warn "Abort signal detected for issue #${issue}"
            return 0
        fi
    fi

    return 1
}

# Check for interactive prompts in the agent's tmux pane and auto-resolve them.
# Claude Code's plan mode presents an approval prompt that blocks execution when
# no human is present. This function detects the prompt and sends the approval
# keystroke so autonomous agents can proceed.
check_and_resolve_prompts() {
    local session_name="$1"

    # Capture current pane content (silently fail if session gone)
    local pane_content
    pane_content=$(tmux -L "$TMUX_SOCKET" capture-pane -t "$session_name" -p 2>/dev/null || true)

    if [[ -z "$pane_content" ]]; then
        return 1
    fi

    # Detect Claude Code plan mode approval prompt.
    # The prompt shows numbered options like:
    #   "Would you like to proceed?"
    #   1. Yes, clear context and bypass permissions
    #   2. Yes, and bypass permissions
    # We look for the distinctive "Would you like to proceed" text.
    if echo "$pane_content" | grep -q "Would you like to proceed"; then
        log_info "Plan mode approval prompt detected in $session_name - auto-approving"
        # Send "1" to select "Yes, clear context and bypass permissions"
        tmux -L "$TMUX_SOCKET" send-keys -t "$session_name" "1" C-m
        return 0
    fi

    return 1
}

main() {
    local name=""
    local timeout="3600"
    local poll_interval="$DEFAULT_SIGNAL_POLL"
    local issue=""
    local json_output=false

    if [[ $# -lt 1 ]]; then
        show_help
        exit 2
    fi

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
            --issue)
                issue="$2"
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
                echo "Unknown argument: $1" >&2
                exit 2
                ;;
        esac
    done

    log_info "Waiting for agent '$name' with signal checking (poll: ${poll_interval}s, timeout: ${timeout}s)"

    # Launch agent-wait.sh in the background
    "${SCRIPT_DIR}/agent-wait.sh" "$name" --timeout "$timeout" --poll-interval "$poll_interval" --json &
    local wait_pid=$!

    local start_time
    start_time=$(date +%s)

    local session_name="${SESSION_PREFIX}${name}"
    local prompt_resolved=false

    # Poll for signals and interactive prompts while background process runs
    while true; do
        # Check for interactive prompts that need auto-approval (e.g., plan mode).
        # Only attempt once to avoid sending stray keystrokes after the prompt clears.
        if [[ "$prompt_resolved" != "true" ]]; then
            if check_and_resolve_prompts "$session_name"; then
                prompt_resolved=true
            fi
        fi

        # Check if agent-wait.sh has finished
        if ! kill -0 "$wait_pid" 2>/dev/null; then
            # Process exited, get its exit code
            wait "$wait_pid"
            local exit_code=$?

            if [[ "$json_output" == "true" ]]; then
                # agent-wait.sh already output JSON, just pass through exit code
                :
            fi
            exit "$exit_code"
        fi

        # Check for shutdown signals
        if check_signals "$issue"; then
            local elapsed=$(( $(date +%s) - start_time ))

            # Kill the background wait process
            kill "$wait_pid" 2>/dev/null || true
            wait "$wait_pid" 2>/dev/null || true

            if [[ "$json_output" == "true" ]]; then
                local signal_type="shutdown"
                if [ -n "$issue" ]; then
                    local labels
                    labels=$(gh issue view "$issue" --json labels --jq '.labels[].name' 2>/dev/null || true)
                    if echo "$labels" | grep -q "loom:abort"; then
                        signal_type="abort"
                    fi
                fi
                echo "{\"status\":\"signal\",\"name\":\"$name\",\"signal_type\":\"$signal_type\",\"elapsed\":$elapsed}"
            else
                log_warn "Shutdown signal detected after ${elapsed}s - aborting wait for '$name'"
            fi
            exit 3
        fi

        sleep "$poll_interval"
    done
}

main "$@"
