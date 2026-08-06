#!/usr/bin/env bash
# Stop the daemon

set -e

DAEMON_PID_FILE=".loom/.daemon.pid"

if [ ! -f "$DAEMON_PID_FILE" ]; then
  echo "No daemon PID file found"

  # Best-effort fallback: look for a dev-mode cargo-built daemon by its real
  # build output path, not a bare name match. `pgrep -f "loom-daemon"` (the
  # previous check here) is a full-cmdline SUBSTRING match against every
  # process on the box -- it matches ANY process whose command line contains
  # that text, including a leaked test fixture literally named `loom-daemon`
  # (e.g. a stray `bash /tmp/xxx/loom-daemon` orphaned by an interrupted test
  # run kept looking like a live daemon for 66 minutes on a real host, #5548).
  # Anchoring to the actual `target/debug/loom-daemon` build path (which a
  # $TMPDIR test fixture does not live under) closes that hole. This is still
  # a best-effort pgrep, not a true liveness check (a PID file / launchd /
  # systemd unit state is) -- acceptable here because this is a local dev
  # convenience script, not a production liveness probe.
  DAEMON_PID=$(pgrep -f '(^|/)target/debug/loom-daemon$' | head -1 || true)
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
