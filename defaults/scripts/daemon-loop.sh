#!/usr/bin/env bash
# Loom Daemon Loop - Shell script wrapper for robust continuous operation
#
# This script implements the "thin parent loop" from loom.md in bash,
# delegating iteration work to Claude via the /loom iterate command.
#
# Usage:
#   ./.loom/scripts/daemon-loop.sh [--force] [--status]
#
# Options:
#   --force    Enable force mode for aggressive autonomous development
#   --status   Check if daemon loop is running
#
# Environment Variables:
#   LOOM_POLL_INTERVAL - Seconds between iterations (default: 120)
#   LOOM_ITERATION_TIMEOUT - Max seconds per iteration (default: 300)
#
# Features:
#   - Deterministic loop behavior (no LLM interpretation variability)
#   - Configurable poll interval via environment variable
#   - Timeout protection prevents hung iterations
#   - All output logged to .loom/daemon.log
#   - Graceful shutdown via .loom/stop-daemon signal file
#   - Session state rotation on startup
#   - Force mode support passed to iterations
#   - PID file prevents multiple instances (.loom/daemon-loop.pid)
#
# Example:
#   # Start daemon with default settings
#   ./.loom/scripts/daemon-loop.sh
#
#   # Start in force mode with custom interval
#   LOOM_POLL_INTERVAL=60 ./.loom/scripts/daemon-loop.sh --force
#
#   # Run in background
#   nohup ./.loom/scripts/daemon-loop.sh --force > /dev/null 2>&1 &
#
#   # Check if daemon is running
#   ./.loom/scripts/daemon-loop.sh --status
#
#   # Stop daemon gracefully
#   touch .loom/stop-daemon

set -euo pipefail

# Configuration
POLL_INTERVAL="${LOOM_POLL_INTERVAL:-120}"
ITERATION_TIMEOUT="${LOOM_ITERATION_TIMEOUT:-300}"
LOG_FILE=".loom/daemon.log"
STATE_FILE=".loom/daemon-state.json"
STOP_SIGNAL=".loom/stop-daemon"
PID_FILE=".loom/daemon-loop.pid"

# ANSI colors (disabled if not a terminal)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    BLUE='\033[0;34m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    BLUE=''
    YELLOW=''
    CYAN=''
    NC=''
fi

# Parse arguments
FORCE_FLAG=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --force|-f)
            FORCE_FLAG="--force"
            shift
            ;;
        --status)
            if [[ -f "$PID_FILE" ]]; then
                pid=$(cat "$PID_FILE")
                if kill -0 "$pid" 2>/dev/null; then
                    echo -e "${GREEN}Daemon loop running (PID: $pid)${NC}"
                    exit 0
                else
                    echo -e "${YELLOW}Daemon loop not running (stale PID file)${NC}"
                    rm -f "$PID_FILE"
                    exit 1
                fi
            else
                echo "Daemon loop not running"
                exit 1
            fi
            ;;
        --help|-h)
            echo "Usage: $0 [--force] [--status]"
            echo ""
            echo "Options:"
            echo "  --force, -f    Enable force mode for aggressive autonomous development"
            echo "  --status       Check if daemon loop is running"
            echo "  --help, -h     Show this help message"
            echo ""
            echo "Environment Variables:"
            echo "  LOOM_POLL_INTERVAL      Seconds between iterations (default: 120)"
            echo "  LOOM_ITERATION_TIMEOUT  Max seconds per iteration (default: 300)"
            echo ""
            echo "To stop the daemon gracefully:"
            echo "  touch .loom/stop-daemon"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Use --help for usage information" >&2
            exit 1
            ;;
    esac
done

# Ensure we're in a git repository with .loom directory
if [[ ! -d ".loom" ]]; then
    echo -e "${RED}Error: .loom directory not found${NC}" >&2
    echo "Run this script from a Loom-enabled repository root" >&2
    exit 1
fi

# Check for existing daemon instance
if [[ -f "$PID_FILE" ]]; then
    existing_pid=$(cat "$PID_FILE")
    if kill -0 "$existing_pid" 2>/dev/null; then
        echo -e "${RED}Error: Daemon loop already running (PID: $existing_pid)${NC}" >&2
        echo "Use --status to check status or stop the existing daemon first" >&2
        exit 1
    else
        echo -e "${YELLOW}Removing stale PID file${NC}"
        rm -f "$PID_FILE"
    fi
fi

# Write PID file
echo $$ > "$PID_FILE"

# Check for claude CLI
if ! command -v claude &> /dev/null; then
    echo -e "${RED}Error: 'claude' CLI not found in PATH${NC}" >&2
    echo "Install Claude Code CLI: https://claude.ai/code" >&2
    exit 1
fi

# Rotate existing state file if present
if [[ -f "./.loom/scripts/rotate-daemon-state.sh" ]] && [[ -f "$STATE_FILE" ]]; then
    echo -e "${BLUE}Rotating previous daemon state...${NC}"
    ./.loom/scripts/rotate-daemon-state.sh 2>/dev/null || true
fi

# Create log directory if needed
mkdir -p "$(dirname "$LOG_FILE")"

# Log function that writes to both console and file
log() {
    local timestamp
    timestamp=$(date -Iseconds)
    echo -e "$timestamp $*" | tee -a "$LOG_FILE"
}

log_header() {
    echo "" | tee -a "$LOG_FILE"
    echo "═══════════════════════════════════════════════════════════════════" | tee -a "$LOG_FILE"
    echo -e "${CYAN}  LOOM DAEMON - SHELL SCRIPT WRAPPER MODE${NC}" | tee -a "$LOG_FILE"
    echo "═══════════════════════════════════════════════════════════════════" | tee -a "$LOG_FILE"
    echo "  Started: $(date -Iseconds)" | tee -a "$LOG_FILE"
    echo "  PID: $$" | tee -a "$LOG_FILE"
    echo "  Mode: ${FORCE_FLAG:-Normal}" | tee -a "$LOG_FILE"
    echo "  Poll interval: ${POLL_INTERVAL}s" | tee -a "$LOG_FILE"
    echo "  Iteration timeout: ${ITERATION_TIMEOUT}s" | tee -a "$LOG_FILE"
    echo "  PID file: $PID_FILE" | tee -a "$LOG_FILE"
    echo "  Stop signal: $STOP_SIGNAL" | tee -a "$LOG_FILE"
    echo "═══════════════════════════════════════════════════════════════════" | tee -a "$LOG_FILE"
    echo "" | tee -a "$LOG_FILE"
}

# Cleanup function called on exit
cleanup() {
    local exit_code=$?
    echo "" | tee -a "$LOG_FILE"
    log "${YELLOW}Daemon loop terminated (exit code: $exit_code)${NC}"
    rm -f "$STOP_SIGNAL"
    rm -f "$PID_FILE"

    # Update state file to mark as not running
    if [[ -f "$STATE_FILE" ]]; then
        # Use a temp file to avoid corrupting state on write failure
        local temp_file
        temp_file=$(mktemp)
        if jq '.running = false | .stopped_at = "'"$(date -Iseconds)"'"' "$STATE_FILE" > "$temp_file" 2>/dev/null; then
            mv "$temp_file" "$STATE_FILE"
        else
            rm -f "$temp_file"
        fi
    fi
}

trap cleanup EXIT SIGINT SIGTERM

# Clear any existing stop signal
rm -f "$STOP_SIGNAL"

# Print startup header
log_header

iteration=0

# Main loop
while true; do
    iteration=$((iteration + 1))

    # Check for stop signal
    if [[ -f "$STOP_SIGNAL" ]]; then
        log "${YELLOW}Iteration $iteration: SHUTDOWN_SIGNAL detected${NC}"
        break
    fi

    # Run iteration via Claude
    timestamp=$(date -Iseconds)
    log "${BLUE}Iteration $iteration: Starting...${NC}"

    # Build the command
    ITERATE_CMD="/loom iterate"
    if [[ -n "$FORCE_FLAG" ]]; then
        ITERATE_CMD="$ITERATE_CMD $FORCE_FLAG"
    fi

    # Capture iteration output with timeout
    # Using timeout command to prevent hung iterations
    # --print flag outputs the response without interactive UI
    if output=$(timeout "$ITERATION_TIMEOUT" claude --print "$ITERATE_CMD" 2>&1); then
        # Extract the summary line (looks for ready=X building=Y pattern)
        summary=$(echo "$output" | grep -E '^ready=' | tail -1 || echo "")

        # If no summary pattern found, look for other indicators
        if [[ -z "$summary" ]]; then
            if echo "$output" | grep -qi "shutdown"; then
                summary="SHUTDOWN_SIGNAL"
            elif echo "$output" | grep -qi "error"; then
                # Extract first error line
                summary="ERROR: $(echo "$output" | grep -i "error" | head -1 | cut -c1-80)"
            elif echo "$output" | grep -qi "complete\|success\|done"; then
                summary="completed"
            else
                # Take last non-empty line as summary
                summary=$(echo "$output" | grep -v '^$' | tail -1 | cut -c1-80 || echo "no output")
            fi
        fi
    else
        exit_code=$?
        if [[ $exit_code -eq 124 ]]; then
            summary="TIMEOUT (iteration exceeded ${ITERATION_TIMEOUT}s)"
            log "${RED}Iteration $iteration: $summary${NC}"
        else
            summary="ERROR (exit code: $exit_code)"
            log "${RED}Iteration $iteration: $summary${NC}"
        fi
    fi

    # Log the summary
    if [[ "$summary" == *"SHUTDOWN"* ]]; then
        log "${YELLOW}Iteration $iteration: $summary${NC}"
        break
    elif [[ "$summary" == *"ERROR"* ]] || [[ "$summary" == *"TIMEOUT"* ]]; then
        log "${RED}Iteration $iteration: $summary${NC}"
    else
        log "${GREEN}Iteration $iteration: $summary${NC}"
    fi

    # Check for stop signal again before sleeping
    if [[ -f "$STOP_SIGNAL" ]]; then
        log "${YELLOW}SHUTDOWN_SIGNAL detected after iteration${NC}"
        break
    fi

    # Sleep before next iteration
    log "Sleeping ${POLL_INTERVAL}s until next iteration..."
    sleep "$POLL_INTERVAL"
done

log "${GREEN}Daemon loop completed gracefully${NC}"
