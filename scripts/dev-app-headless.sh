#!/usr/bin/env bash
# Development mode for non-TTY environments (Claude Code, CI, etc.)
# Starts daemon in background, Tauri in foreground

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color
BOLD='\033[1m'

# Log file locations
DAEMON_LOG="$HOME/.loom/daemon-dev.log"
DAEMON_PID_FILE="$HOME/.loom/daemon-dev.pid"

# Cleanup function
cleanup() {
    if [ -f "$DAEMON_PID_FILE" ]; then
        DAEMON_PID=$(cat "$DAEMON_PID_FILE")
        if ps -p "$DAEMON_PID" > /dev/null 2>&1; then
            echo -e "\n${YELLOW}Stopping daemon (PID: $DAEMON_PID)...${NC}"
            kill "$DAEMON_PID" 2>/dev/null || true
            # Wait for graceful shutdown
            for i in {1..10}; do
                if ! ps -p "$DAEMON_PID" > /dev/null 2>&1; then
                    break
                fi
                sleep 0.5
            done
            # Force kill if still running
            if ps -p "$DAEMON_PID" > /dev/null 2>&1; then
                echo -e "${YELLOW}Force killing daemon...${NC}"
                kill -9 "$DAEMON_PID" 2>/dev/null || true
            fi
        fi
        rm -f "$DAEMON_PID_FILE"
    fi
    echo -e "${GREEN}✓ Cleanup complete${NC}"
}

# Set trap to cleanup on exit
trap cleanup EXIT INT TERM

echo -e "${BOLD}${CYAN}╔════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${CYAN}║   Loom Development Mode (Headless)    ║${NC}"
echo -e "${BOLD}${CYAN}╚════════════════════════════════════════╝${NC}"
echo ""

# Ensure .loom directory exists
mkdir -p "$HOME/.loom"

# Setup MCP first
echo -e "${BLUE}Setting up MCP servers...${NC}"
./scripts/setup-mcp.sh > /dev/null 2>&1 || true

# Start daemon in background
echo -e "${BLUE}Starting daemon in background...${NC}"
echo -e "${CYAN}  Log file: $DAEMON_LOG${NC}"

# Start daemon and capture PID
pnpm daemon:dev > "$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
echo "$DAEMON_PID" > "$DAEMON_PID_FILE"

echo -e "${GREEN}✓ Daemon started (PID: $DAEMON_PID)${NC}"

# Wait for daemon to be ready (check for socket file or log message)
echo -e "${BLUE}Waiting for daemon to initialize...${NC}"
DAEMON_READY=false
for i in {1..30}; do
    # Check if daemon is still running
    if ! ps -p "$DAEMON_PID" > /dev/null 2>&1; then
        echo -e "${RED}✗ Daemon failed to start${NC}"
        echo -e "${YELLOW}Last 20 lines of daemon log:${NC}"
        tail -n 20 "$DAEMON_LOG"
        exit 1
    fi

    # Check if daemon socket exists (assuming it creates one in ~/.loom/loom-daemon.sock)
    if [ -S "$HOME/.loom/loom-daemon.sock" ]; then
        DAEMON_READY=true
        break
    fi

    # Alternative: check for "Listening" or similar message in log
    if grep -q "Listening\|Started\|Ready" "$DAEMON_LOG" 2>/dev/null; then
        DAEMON_READY=true
        break
    fi

    sleep 0.5
done

if [ "$DAEMON_READY" = true ]; then
    echo -e "${GREEN}✓ Daemon ready${NC}"
else
    echo -e "${YELLOW}⚠ Daemon may still be starting (waited 15s)${NC}"
    echo -e "${CYAN}  Check log file if issues occur: $DAEMON_LOG${NC}"
fi

echo ""
echo -e "${BLUE}Starting Tauri development server...${NC}"
echo -e "${CYAN}  (This will show Tauri output in the foreground)${NC}"
echo ""
echo -e "${YELLOW}Keyboard shortcuts:${NC}"
echo -e "  Ctrl+C - Stop both daemon and Tauri"
echo ""
echo -e "${YELLOW}To monitor daemon:${NC}"
echo -e "  tail -f $DAEMON_LOG"
echo ""

# Give a moment for user to read
sleep 2

# Start Tauri in foreground
# This will show all Tauri output directly
pnpm tauri:dev

# Note: cleanup() will be called automatically on exit due to trap
