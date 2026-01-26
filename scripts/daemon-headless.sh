#!/usr/bin/env bash
# Fully headless daemon mode for AI/automated testing
# Starts daemon in background with minimal output

set -e

DAEMON_PID_FILE=".loom/.daemon.pid"
DAEMON_LOG_FILE=".loom/.daemon.log"
DAEMON_SOCKET="$HOME/.loom/loom-daemon.sock"

echo "Starting daemon in headless mode..."

# Clean up any existing daemon
./scripts/stop-daemon.sh > /dev/null 2>&1 || true
sleep 1

# Clean up orphaned tmux sessions
./scripts/clean-tmux.sh > /dev/null 2>&1 || true

# Start daemon
./scripts/start-daemon.sh > /dev/null 2>&1

if [ ! -f "$DAEMON_PID_FILE" ]; then
  echo "ERROR: Failed to start daemon"
  exit 1
fi

DAEMON_PID=$(cat "$DAEMON_PID_FILE")
echo "✓ Daemon started (PID: $DAEMON_PID)"
echo "  Socket: $DAEMON_SOCKET"
echo "  Logs: $DAEMON_LOG_FILE"
echo ""
echo "Use MCP tools to interact with the daemon:"
echo "  - mcp__loom__list_terminals"
echo "  - mcp__loom__get_terminal_output"
echo "  - mcp__loom__read_state_file"
echo "  - mcp__loom__tail_daemon_log"
echo ""
echo "To stop: pnpm daemon:stop"
