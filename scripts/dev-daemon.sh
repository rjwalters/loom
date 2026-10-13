#!/usr/bin/env bash
# Interactive daemon development mode
# Starts daemon in background and monitors its activity
#
# Usage: scripts/dev-daemon.sh [--autonomy]
#
# Autonomy (work finder, role runner, claim reconciliation, auto-update) is OFF
# by default (#9703): a daemon in a clone of rjwalters/loom acts on
# rjwalters/loom. Pass --autonomy only on a checkout you maintain.

set -e

if [ "${1:-}" = "--autonomy" ]; then AUTONOMY="ON (--autonomy given)"; else
  AUTONOMY="OFF (dev default; rerun with --autonomy to enable)"
  export LOOM_WORK_FINDER=0 LOOM_ROLE_RUNNER=0 LOOM_STALE_CLAIM_RECONCILE=0 LOOM_AUTO_UPDATE=0
fi

DAEMON_PID_FILE=".loom/.daemon.pid"
DAEMON_LOG_FILE=".loom/.daemon.log"
DAEMON_SOCKET="$HOME/.loom/daemon.sock"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
GRAY='\033[0;90m'
NC='\033[0m' # No Color
BOLD='\033[1m'

# Cleanup on exit
cleanup() {
  echo ""
  echo -e "${YELLOW}Stopping daemon...${NC}"
  ./scripts/stop-daemon.sh > /dev/null 2>&1 || true
  exit 0
}

trap cleanup INT TERM

# Clear screen and show header
clear
echo -e "${BOLD}${CYAN}╔════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${CYAN}║     Loom Daemon - Development Mode    ║${NC}"
echo -e "${BOLD}${CYAN}╚════════════════════════════════════════╝${NC}"
echo ""

# Clean up any orphaned tmux sessions for consistent test state
echo -e "${BLUE}🧹 Cleaning orphaned tmux sessions...${NC}"
./scripts/clean-tmux.sh
echo ""

# Check if daemon already running
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    echo -e "${YELLOW}⚠ Daemon already running (PID: $PID)${NC}"
    echo -e "${YELLOW}  Stopping existing daemon first...${NC}"
    ./scripts/stop-daemon.sh > /dev/null 2>&1 || true
    sleep 1
  fi
fi

# Start daemon
echo -e "${BLUE}🚀 Starting daemon: autonomy ${AUTONOMY}${NC}"
./scripts/start-daemon.sh > /dev/null 2>&1

if [ ! -f "$DAEMON_PID_FILE" ]; then
  echo -e "${RED}✗ Failed to start daemon${NC}"
  exit 1
fi

DAEMON_PID=$(cat "$DAEMON_PID_FILE")
echo -e "${GREEN}✓ Daemon started (PID: $DAEMON_PID)${NC}"
echo -e "${GRAY}  Socket: $DAEMON_SOCKET${NC}"
echo -e "${GRAY}  Logs: $DAEMON_LOG_FILE${NC}"
echo ""

# Function to get connection count (connections to the socket)
get_connection_count() { lsof "$DAEMON_SOCKET" 2>/dev/null | grep -v COMMAND | wc -l | tr -d ' ' || echo "0"; }

# Function to get terminal count from logs
get_terminal_count() {
  grep -i "restored.*terminals" "$DAEMON_LOG_FILE" 2>/dev/null | tail -1 | sed -n 's/.*Restored \([0-9]*\) terminals.*/\1/p' || echo "0"
}

# Functions to count ERROR / WARN lines in the last 50 log lines
get_recent_errors() { tail -50 "$DAEMON_LOG_FILE" 2>/dev/null | grep -c "ERROR" || echo "0"; }
get_recent_warnings() { tail -50 "$DAEMON_LOG_FILE" 2>/dev/null | grep -c "WARN" || echo "0"; }

# Show status bar
show_status() {
  local uptime_seconds=$1
  local connections=$(get_connection_count)
  local terminals=$(get_terminal_count)
  local errors=$(get_recent_errors)
  local warnings=$(get_recent_warnings)

  # Format uptime
  local hours=$((uptime_seconds / 3600))
  local minutes=$(((uptime_seconds % 3600) / 60))
  local seconds=$((uptime_seconds % 60))
  local uptime_str=$(printf "%02d:%02d:%02d" $hours $minutes $seconds)

  # Status line
  echo -ne "\r${BOLD}Status:${NC} "

  if kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo -ne "${GREEN}●${NC} Running  "
  else
    echo -ne "${RED}●${NC} Stopped  "
  fi

  echo -ne "${BOLD}Uptime:${NC} ${uptime_str}  "
  echo -ne "${BOLD}Terminals:${NC} ${terminals}  "
  echo -ne "${BOLD}Connections:${NC} ${connections}  "

  if [ "$errors" -gt 0 ] 2>/dev/null; then
    echo -ne "${RED}Errors:${NC} ${errors}  "
  fi

  if [ "$warnings" -gt 0 ] 2>/dev/null; then
    echo -ne "${YELLOW}Warnings:${NC} ${warnings}  "
  fi

  echo -ne "              " # Clear rest of line
}

echo -e "${BOLD}${CYAN}════════════════════════════════════════${NC}"
echo -e "${BOLD}Live Activity Monitor${NC}"
echo -e "${GRAY}Press Ctrl+C to stop daemon and exit${NC}"
echo -e "${BOLD}${CYAN}════════════════════════════════════════${NC}"
echo ""

# Track start time
START_TIME=$(date +%s)

# Tail logs with filtering and coloring
tail -f "$DAEMON_LOG_FILE" 2>/dev/null | while IFS= read -r line; do
  # Update status bar every line
  CURRENT_TIME=$(date +%s)
  UPTIME=$((CURRENT_TIME - START_TIME))

  # Show status on first line or every 10 seconds
  if [ $((UPTIME % 10)) -eq 0 ] || [ "$UPTIME" -eq 0 ]; then
    show_status "$UPTIME"
    echo "" # New line after status
  fi

  # Color code the log lines
  if echo "$line" | grep -q "ERROR"; then
    echo -e "${RED}${line}${NC}"
  elif echo "$line" | grep -q "WARN"; then
    echo -e "${YELLOW}${line}${NC}"
  elif echo "$line" | grep -q "INFO.*Restored.*terminals"; then
    echo -e "${GREEN}${line}${NC}"
  elif echo "$line" | grep -q "INFO.*listening"; then
    echo -e "${GREEN}${line}${NC}"
  elif echo "$line" | grep -q "Client connected\|New client"; then
    echo -e "${CYAN}${line}${NC}"
  elif echo "$line" | grep -q "Client disconnected"; then
    echo -e "${GRAY}${line}${NC}"
  elif echo "$line" | grep -q "CreateTerminal\|DestroyTerminal"; then
    echo -e "${BLUE}${line}${NC}"
  elif echo "$line" | grep -q "SendInput\|GetTerminalOutput"; then
    # Skip high-frequency polling messages (or show in gray)
    echo -e "${GRAY}${line}${NC}"
  else
    echo "$line"
  fi
done &

TAIL_PID=$!

# Wait for Ctrl+C
wait $TAIL_PID
