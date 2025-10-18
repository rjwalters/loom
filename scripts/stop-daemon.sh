#!/usr/bin/env bash
# Stop the daemon

set -e

DAEMON_PID_FILE=".loom/.daemon.pid"

if [ ! -f "$DAEMON_PID_FILE" ]; then
  echo "No daemon PID file found"

  # Try to find daemon process anyway
  DAEMON_PID=$(pgrep -f "loom-daemon" | head -1 || true)
  if [ -n "$DAEMON_PID" ]; then
    echo "Found daemon process (PID: $DAEMON_PID)"
    kill "$DAEMON_PID" || true
    echo "Daemon stopped"
  else
    echo "Daemon not running"
  fi
  exit 0
fi

PID=$(cat "$DAEMON_PID_FILE")

if kill -0 "$PID" 2>/dev/null; then
  echo "Stopping daemon (PID: $PID)..."
  kill "$PID"

  # Wait for process to die (up to 5 seconds)
  for i in {1..50}; do
    if ! kill -0 "$PID" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done

  # Force kill if still running
  if kill -0 "$PID" 2>/dev/null; then
    echo "Force killing daemon..."
    kill -9 "$PID" || true
  fi

  echo "Daemon stopped"
else
  echo "Daemon not running (stale PID file)"
fi

rm -f "$DAEMON_PID_FILE"
