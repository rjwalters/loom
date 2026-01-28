#!/bin/bash
# agent-wait-bg.sh - Wait for a tmux Claude agent with shutdown signal checking
#
# Wraps agent-wait.sh to run in the background while polling for shutdown signals.
# This allows shepherds to detect shutdown/abort requests during long waits
# instead of blocking until the phase completes.
#
# Also includes stuck detection with configurable thresholds to identify agents
# that appear unresponsive or making no progress.
#
# Exit codes:
#   0 - Agent completed (same as agent-wait.sh)
#   1 - Timeout reached (same as agent-wait.sh)
#   2 - Session not found (same as agent-wait.sh)
#   3 - Shutdown signal detected during wait
#   4 - Agent stuck and intervention triggered (pause/restart)
#
# Stuck Detection Environment Variables:
#   LOOM_STUCK_WARNING   - Seconds without progress before warning (default: 300)
#   LOOM_STUCK_CRITICAL  - Seconds without progress before critical (default: 600)
#   LOOM_STUCK_ACTION    - Action on stuck: warn, pause, restart (default: warn)
#
# Usage:
#   agent-wait-bg.sh <name> [--timeout <s>] [--poll-interval <s>] [--issue <N>] [--json]
#
# Examples:
#   agent-wait-bg.sh builder-issue-42 --timeout 1800 --issue 42
#   agent-wait-bg.sh shepherd-1 --poll-interval 10 --json
#   LOOM_STUCK_WARNING=180 LOOM_STUCK_ACTION=pause agent-wait-bg.sh builder-1

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# tmux configuration (must match agent-spawn.sh)
TMUX_SOCKET="loom"
SESSION_PREFIX="loom-"

# Colors (RED unused but kept for consistency with other scripts and future error logging)
# shellcheck disable=SC2034
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$(date '+%H:%M:%S')]${NC} $*" >&2; }
log_success() { echo -e "${GREEN}[$(date '+%H:%M:%S')] ✓${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[$(date '+%H:%M:%S')] ⚠${NC} $*" >&2; }

# Default poll interval for signal checking
DEFAULT_SIGNAL_POLL=5

# Grace period (seconds) to wait after detecting completion before force-terminating
DEFAULT_GRACE_PERIOD=30

# Stuck detection thresholds (configurable via environment variables)
STUCK_WARNING_THRESHOLD=${LOOM_STUCK_WARNING:-300}   # 5 min default
STUCK_CRITICAL_THRESHOLD=${LOOM_STUCK_CRITICAL:-600} # 10 min default
STUCK_ACTION=${LOOM_STUCK_ACTION:-warn}              # warn, pause, restart

# Progress tracking file prefix
PROGRESS_DIR="/tmp/loom-agent-progress"

show_help() {
    cat <<EOF
${BLUE}agent-wait-bg.sh - Wait for agent with shutdown signal checking${NC}

${YELLOW}USAGE:${NC}
    agent-wait-bg.sh <name> [OPTIONS]

${YELLOW}OPTIONS:${NC}
    --timeout <seconds>        Maximum time to wait (default: 3600)
    --poll-interval <seconds>  Time between signal checks (default: $DEFAULT_SIGNAL_POLL)
    --issue <N>                Issue number for per-issue abort checking
    --grace-period <seconds>   Time to wait after completion detection (default: $DEFAULT_GRACE_PERIOD)
    --json                     Output result as JSON
    --help                     Show this help message

${YELLOW}EXIT CODES:${NC}
    0  Agent completed
    1  Timeout reached
    2  Session not found
    3  Shutdown signal detected
    4  Agent stuck and intervention triggered

${YELLOW}SIGNALS CHECKED:${NC}
    - .loom/stop-shepherds file (global shepherd shutdown)
    - loom:abort label on issue (per-issue abort, requires --issue)

${YELLOW}COMPLETION DETECTION:${NC}
    As a backup to /exit, monitors logs for role-specific completion patterns:
    - Builder: PR created with loom:review-requested
    - Judge: PR labeled with loom:pr or loom:changes-requested
    - Doctor: PR fixed and labeled with loom:review-requested
    - Curator: Issue labeled with loom:curated

${YELLOW}STUCK DETECTION:${NC}
    Monitors agent progress by tracking tmux pane content changes.
    Configure via environment variables:

    LOOM_STUCK_WARNING   Seconds without progress before warning (default: 300)
    LOOM_STUCK_CRITICAL  Seconds without progress before critical (default: 600)
    LOOM_STUCK_ACTION    Action on stuck: warn, pause, restart (default: warn)

${YELLOW}EXAMPLES:${NC}
    agent-wait-bg.sh builder-issue-42 --timeout 1800 --issue 42
    agent-wait-bg.sh curator-issue-10 --poll-interval 10 --json

    # With custom stuck thresholds
    LOOM_STUCK_WARNING=180 LOOM_STUCK_ACTION=pause agent-wait-bg.sh builder-1

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

# Initialize progress tracking for an agent
init_progress_tracking() {
    local name="$1"

    mkdir -p "$PROGRESS_DIR"

    local progress_file="$PROGRESS_DIR/${name}"
    local hash_file="${progress_file}.hash"
    local time_file="${progress_file}.time"

    # Initialize with current time as last progress
    date +%s > "$time_file"
    # Clear any existing hash
    rm -f "$hash_file"
}

# Check for progress by comparing tmux pane content hash
# Returns 0 if progress detected, 1 if no change
check_progress() {
    local name="$1"
    local session_name="$2"

    local progress_file="$PROGRESS_DIR/${name}"
    local hash_file="${progress_file}.hash"
    local time_file="${progress_file}.time"

    # Capture current pane content and hash it
    local current_content
    current_content=$(tmux -L "$TMUX_SOCKET" capture-pane -t "$session_name" -p 2>/dev/null || echo "")

    if [[ -z "$current_content" ]]; then
        # Can't capture pane - session may be gone
        return 1
    fi

    local current_hash
    current_hash=$(echo "$current_content" | md5 -q 2>/dev/null || echo "$current_content" | md5sum 2>/dev/null | cut -d' ' -f1)

    local last_hash=""
    if [[ -f "$hash_file" ]]; then
        last_hash=$(cat "$hash_file" 2>/dev/null || echo "")
    fi

    if [[ "$current_hash" != "$last_hash" ]]; then
        # Content changed - progress detected
        echo "$current_hash" > "$hash_file"
        date +%s > "$time_file"
        return 0  # Progress detected
    fi

    return 1  # No progress
}

# Get idle time (seconds since last progress)
get_idle_time() {
    local name="$1"

    local time_file="$PROGRESS_DIR/${name}.time"

    if [[ ! -f "$time_file" ]]; then
        echo "0"
        return
    fi

    local last_progress
    last_progress=$(cat "$time_file" 2>/dev/null || echo "0")
    local now
    now=$(date +%s)

    echo $((now - last_progress))
}

# Check if agent is stuck and return status
# Returns: OK, WARNING, or CRITICAL
check_stuck_status() {
    local name="$1"

    local idle_time
    idle_time=$(get_idle_time "$name")

    if [[ "$idle_time" -gt "$STUCK_CRITICAL_THRESHOLD" ]]; then
        echo "CRITICAL"
    elif [[ "$idle_time" -gt "$STUCK_WARNING_THRESHOLD" ]]; then
        echo "WARNING"
    else
        echo "OK"
    fi
}

# Handle stuck agent intervention
# Returns 0 if should continue waiting, 1 if should exit
handle_stuck() {
    local name="$1"
    local session_name="$2"
    local status="$3"
    local issue="$4"
    local json_output="$5"
    local elapsed="$6"

    local idle_time
    idle_time=$(get_idle_time "$name")

    case "$STUCK_ACTION" in
        warn)
            if [[ "$status" == "CRITICAL" ]]; then
                log_warn "CRITICAL: Agent '$name' appears stuck (no progress for ${idle_time}s)"
            else
                log_warn "WARNING: Agent '$name' may be stuck (no progress for ${idle_time}s)"
            fi
            return 0  # Continue waiting
            ;;
        pause)
            log_warn "PAUSE: Pausing stuck agent '$name' (no progress for ${idle_time}s)"

            # Signal the agent to pause via .loom/signals
            local signal_file="${REPO_ROOT}/.loom/signals/pause-${name}"
            mkdir -p "${REPO_ROOT}/.loom/signals"
            echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) - Auto-paused: stuck detection (idle ${idle_time}s)" > "$signal_file"

            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"stuck\",\"name\":\"$name\",\"action\":\"paused\",\"idle_time\":$idle_time,\"stuck_status\":\"$status\",\"elapsed\":$elapsed}"
            fi
            return 1  # Exit with stuck status
            ;;
        restart)
            log_warn "RESTART: Restarting stuck agent '$name' (no progress for ${idle_time}s)"

            # Destroy the tmux session
            tmux -L "$TMUX_SOCKET" kill-session -t "$session_name" 2>/dev/null || true

            # Clean up progress files
            cleanup_progress_files "$name"

            if [[ "$json_output" == "true" ]]; then
                echo "{\"status\":\"stuck\",\"name\":\"$name\",\"action\":\"restarted\",\"idle_time\":$idle_time,\"stuck_status\":\"$status\",\"elapsed\":$elapsed}"
            fi
            return 1  # Exit with stuck status (shepherd will respawn)
            ;;
        *)
            # Unknown action, default to warn
            log_warn "Agent '$name' stuck status: $status (idle ${idle_time}s)"
            return 0
            ;;
    esac
}

# Clean up progress tracking files for an agent
cleanup_progress_files() {
    local name="$1"

    rm -f "$PROGRESS_DIR/${name}.hash"
    rm -f "$PROGRESS_DIR/${name}.time"
    rm -f "$PROGRESS_DIR/${name}"
}

# Extract phase from session name (e.g., "builder-issue-123" -> "builder")
# Returns empty string if no recognized phase found
extract_phase_from_session() {
    local session_name="$1"

    # Remove the "loom-" prefix if present (session_name may be full tmux session name)
    local base_name="${session_name#loom-}"

    # Extract the first component before "-issue-" or "-"
    local phase
    phase=$(echo "$base_name" | sed -E 's/^(builder|judge|curator|doctor|shepherd)-.*$/\1/')

    # Verify it's a recognized phase
    case "$phase" in
        builder|judge|curator|doctor|shepherd)
            echo "$phase"
            ;;
        *)
            echo ""
            ;;
    esac
}

# Check for role-specific completion patterns in log file
# Returns 0 if completion detected, 1 otherwise
# Sets COMPLETION_REASON global variable with the detected pattern
#
# The function is phase-aware: it only checks patterns relevant to the
# current phase (extracted from session name) to avoid false matches.
# For example, a judge reviewing a PR with loom:review-requested won't
# incorrectly match the builder_pr_created pattern.
check_completion_patterns() {
    local session_name="$1"
    local log_file="${REPO_ROOT}/.loom/logs/${session_name}.log"

    if [[ ! -f "$log_file" ]]; then
        return 1
    fi

    # Get recent log content (last 100 lines to check for completion)
    local recent_log
    recent_log=$(tail -100 "$log_file" 2>/dev/null || true)

    if [[ -z "$recent_log" ]]; then
        return 1
    fi

    # Extract phase from session name for phase-aware pattern matching
    local phase
    phase=$(extract_phase_from_session "$session_name")

    # Generic completion: /exit command detected (always checked regardless of phase)
    # More robust pattern to catch various prompt styles and formatting
    # Including indented /exit from LLM text output (e.g., "  /exit")
    if echo "$recent_log" | grep -qE '(^|\s+|❯\s*|>\s*)/exit\s*$'; then
        COMPLETION_REASON="explicit_exit"
        return 0
    fi

    # Phase-specific completion patterns
    # Only check the pattern relevant to the current phase to avoid false matches
    case "$phase" in
        builder)
            # Builder completion: PR created with loom:review-requested
            # Look for patterns like "gh pr create" followed by "loom:review-requested"
            # or explicit PR creation success messages
            if echo "$recent_log" | grep -qE 'loom:review-requested|PR #[0-9]+ created|pull request.*created'; then
                COMPLETION_REASON="builder_pr_created"
                return 0
            fi
            ;;
        judge)
            # Judge completion: PR labeled with loom:pr or loom:changes-requested
            if echo "$recent_log" | grep -qE 'add-label.*loom:pr|add-label.*loom:changes-requested|--add-label "loom:pr"|--add-label "loom:changes-requested"'; then
                COMPLETION_REASON="judge_review_complete"
                return 0
            fi
            ;;
        doctor)
            # Doctor completion: PR labeled with loom:review-requested after fixes
            # Similar to builder but in context of fixing (look for treating label removal)
            if echo "$recent_log" | grep -qE 'remove-label.*loom:treating.*add-label.*loom:review-requested|remove-label.*loom:changes-requested.*add-label.*loom:review-requested'; then
                COMPLETION_REASON="doctor_fixes_complete"
                return 0
            fi
            ;;
        curator)
            # Curator completion: Issue labeled with loom:curated
            if echo "$recent_log" | grep -qE 'add-label.*loom:curated|--add-label "loom:curated"'; then
                COMPLETION_REASON="curator_curation_complete"
                return 0
            fi
            ;;
        *)
            # Unknown phase or shepherd - check all patterns as fallback
            # This handles generic or shepherd sessions that may spawn worker roles
            if echo "$recent_log" | grep -qE 'loom:review-requested|PR #[0-9]+ created|pull request.*created'; then
                COMPLETION_REASON="builder_pr_created"
                return 0
            fi
            if echo "$recent_log" | grep -qE 'add-label.*loom:pr|add-label.*loom:changes-requested|--add-label "loom:pr"|--add-label "loom:changes-requested"'; then
                COMPLETION_REASON="judge_review_complete"
                return 0
            fi
            if echo "$recent_log" | grep -qE 'remove-label.*loom:treating.*add-label.*loom:review-requested|remove-label.*loom:changes-requested.*add-label.*loom:review-requested'; then
                COMPLETION_REASON="doctor_fixes_complete"
                return 0
            fi
            if echo "$recent_log" | grep -qE 'add-label.*loom:curated|--add-label "loom:curated"'; then
                COMPLETION_REASON="curator_curation_complete"
                return 0
            fi
            ;;
    esac

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
    local grace_period="$DEFAULT_GRACE_PERIOD"
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
            --grace-period)
                grace_period="$2"
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
    log_info "Stuck detection: warning=${STUCK_WARNING_THRESHOLD}s, critical=${STUCK_CRITICAL_THRESHOLD}s, action=${STUCK_ACTION}"

    # Launch agent-wait.sh in the background
    "${SCRIPT_DIR}/agent-wait.sh" "$name" --timeout "$timeout" --poll-interval "$poll_interval" --json &
    local wait_pid=$!

    local start_time
    start_time=$(date +%s)

    local session_name="${SESSION_PREFIX}${name}"
    local prompt_resolved=false
    local completion_detected=false
    local completion_time=0
    local stuck_warned=false
    local stuck_critical_reported=false
    COMPLETION_REASON=""

    # Initialize progress tracking
    init_progress_tracking "$name"

    # Poll for signals, prompts, completion patterns, and stuck detection while background process runs
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

            # Clean up progress files on completion
            cleanup_progress_files "$name"

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

            # Clean up progress files
            cleanup_progress_files "$name"

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

        # Check for completion patterns in log (backup detection)
        if [[ "$completion_detected" != "true" ]]; then
            if check_completion_patterns "$session_name"; then
                completion_detected=true
                completion_time=$(date +%s)
                # Only warn about grace period for role-specific patterns, not explicit /exit
                if [[ "$COMPLETION_REASON" != "explicit_exit" ]]; then
                    log_warn "Completion pattern detected ($COMPLETION_REASON) but session still running - waiting ${grace_period}s grace period"
                    log_warn "Agent should have executed /exit after completing task"
                fi
            fi
        fi

        # If completion was detected, check if grace period has elapsed
        if [[ "$completion_detected" == "true" ]]; then
            # /exit is an explicit completion signal - no grace period needed
            if [[ "$COMPLETION_REASON" == "explicit_exit" ]]; then
                local elapsed=$(( $(date +%s) - start_time ))
                log_info "/exit detected in output - sending /exit to prompt and terminating '$session_name'"

                # Send /exit to the actual tmux prompt as backup
                # This ensures the CLI receives /exit even if the LLM just output it as text
                tmux -L "$TMUX_SOCKET" send-keys -t "$session_name" "/exit" C-m 2>/dev/null || true

                # Brief pause to let /exit process
                sleep 1

                # Kill the background wait process
                kill "$wait_pid" 2>/dev/null || true
                wait "$wait_pid" 2>/dev/null || true

                # Clean up progress files
                cleanup_progress_files "$name"

                # Destroy the tmux session to clean up
                tmux -L "$TMUX_SOCKET" kill-session -t "$session_name" 2>/dev/null || true

                if [[ "$json_output" == "true" ]]; then
                    echo "{\"status\":\"completed\",\"name\":\"$name\",\"reason\":\"explicit_exit\",\"elapsed\":$elapsed}"
                else
                    log_success "Agent '$name' completed (explicit /exit after ${elapsed}s)"
                fi
                exit 0
            fi

            local grace_elapsed=$(( $(date +%s) - completion_time ))
            if [[ "$grace_elapsed" -ge "$grace_period" ]]; then
                local elapsed=$(( $(date +%s) - start_time ))
                log_warn "Grace period expired - force-terminating session '$session_name'"

                # Kill the background wait process
                kill "$wait_pid" 2>/dev/null || true
                wait "$wait_pid" 2>/dev/null || true

                # Clean up progress files
                cleanup_progress_files "$name"

                # Destroy the tmux session to clean up
                tmux -L "$TMUX_SOCKET" kill-session -t "$session_name" 2>/dev/null || true

                if [[ "$json_output" == "true" ]]; then
                    echo "{\"status\":\"completed\",\"name\":\"$name\",\"reason\":\"completion_pattern_detected\",\"pattern\":\"$COMPLETION_REASON\",\"elapsed\":$elapsed,\"grace_period_used\":true}"
                else
                    log_success "Agent '$name' completed (pattern: $COMPLETION_REASON, forced after ${grace_period}s grace period)"
                fi
                exit 0
            fi
        fi

        # Check for progress and update stuck tracking
        check_progress "$name" "$session_name" || true

        # Check stuck status (only if not already completing)
        if [[ "$completion_detected" != "true" ]]; then
            local stuck_status
            stuck_status=$(check_stuck_status "$name")

            if [[ "$stuck_status" == "WARNING" ]] && [[ "$stuck_warned" != "true" ]]; then
                stuck_warned=true
                local elapsed=$(( $(date +%s) - start_time ))
                if [[ "$STUCK_ACTION" != "warn" ]]; then
                    # For pause/restart actions, only trigger on CRITICAL
                    log_warn "Agent '$name' showing signs of being stuck (no progress for $(get_idle_time "$name")s)"
                else
                    handle_stuck "$name" "$session_name" "$stuck_status" "$issue" "$json_output" "$elapsed"
                fi
            elif [[ "$stuck_status" == "CRITICAL" ]] && [[ "$stuck_critical_reported" != "true" ]]; then
                stuck_critical_reported=true
                local elapsed=$(( $(date +%s) - start_time ))

                # For pause/restart, trigger intervention at CRITICAL level
                if ! handle_stuck "$name" "$session_name" "$stuck_status" "$issue" "$json_output" "$elapsed"; then
                    # Intervention triggered that requires exit

                    # Kill the background wait process
                    kill "$wait_pid" 2>/dev/null || true
                    wait "$wait_pid" 2>/dev/null || true

                    # Clean up progress files
                    cleanup_progress_files "$name"

                    exit 4
                fi
            fi
        fi

        sleep "$poll_interval"
    done
}

main "$@"
