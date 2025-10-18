#!/usr/bin/env bash
# Start the daemon in the background and store PID

set -e

DAEMON_PID_FILE=".loom/.daemon.pid"
DAEMON_LOG_FILE=".loom/.daemon.log"

# Check if daemon is already running
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    echo "Daemon already running (PID: $PID)"
    exit 0
  else
    echo "Removing stale PID file"
    rm -f "$DAEMON_PID_FILE"
  fi
fi

# Ensure .loom directory exists
mkdir -p .loom

# Start daemon in background
echo "Starting daemon..."
(RUST_LOG=info cargo run --manifest-path=loom-daemon/Cargo.toml > "$DAEMON_LOG_FILE" 2>&1) &
CARGO_PID=$!

# Wait for the actual daemon process to spawn (cargo spawns it as a child)
sleep 2

# Find the actual loom-daemon process (not cargo)
DAEMON_PID=$(pgrep -P "$CARGO_PID" -f "target/debug/loom-daemon" || pgrep -f "target/debug/loom-daemon" | head -1)

if [ -z "$DAEMON_PID" ]; then
  echo "ERROR: Daemon process not found"
  kill "$CARGO_PID" 2>/dev/null || true
  rm -f "$DAEMON_PID_FILE"
  exit 1
fi

# Store PID
echo "$DAEMON_PID" > "$DAEMON_PID_FILE"

# Verify it started
if kill -0 "$DAEMON_PID" 2>/dev/null; then
  echo "Daemon started successfully (PID: $DAEMON_PID)"
  echo "Logs: tail -f $DAEMON_LOG_FILE"
else
  echo "ERROR: Daemon failed to start"
  rm -f "$DAEMON_PID_FILE"
  exit 1
fi
