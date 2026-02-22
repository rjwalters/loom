#!/usr/bin/env bash
# Stop the Loom daemon gracefully.
#
# Writes .loom/stop-daemon (the standard graceful shutdown signal) and
# optionally waits for the daemon to exit.
#
# Usage:
#   ./.loom/scripts/stop-daemon.sh [--wait [N]]
#
# Options:
#   --wait [N]   Wait up to N seconds for daemon to exit (default: 30)
#   --force      Send SIGTERM immediately instead of using stop-daemon file
#   --help, -h   Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PIDFILE="$REPO_ROOT/.loom/daemon-loop.pid"
STOP_SIGNAL="$REPO_ROOT/.loom/stop-daemon"

WAIT=false
WAIT_SECS=30
FORCE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wait)
            WAIT=true
            if [[ "${2:-}" =~ ^[0-9]+$ ]]; then
                WAIT_SECS="$2"
                shift
            fi
            ;;
        --force)  FORCE=true;;
        --help|-h)
            sed -n '/^# Usage:/,/^[^#]/p' "$0" | sed '$d' | sed 's/^# \?//'
            exit 0
            ;;
        *)  echo "Unknown option: $1" >&2; exit 1;;
    esac
    shift
done

# Verify daemon is running
if [[ ! -f "$PIDFILE" ]]; then
    echo "Daemon not running (no PID file)"
    exit 0
fi

PID="$(cat "$PIDFILE")"
if ! kill -0 "$PID" 2>/dev/null; then
    echo "Daemon not running (PID $PID not found)"
    rm -f "$PIDFILE"
    exit 0
fi

if "$FORCE"; then
    echo "Sending SIGTERM to daemon (PID $PID)..."
    kill -TERM "$PID" 2>/dev/null || true
else
    echo "Writing stop signal to $STOP_SIGNAL"
    echo "The daemon will exit after completing its current iteration."
    touch "$STOP_SIGNAL"
fi

if "$WAIT" || "$FORCE"; then
    echo "Waiting up to ${WAIT_SECS}s for daemon to exit..."
    elapsed=0
    while kill -0 "$PID" 2>/dev/null; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $WAIT_SECS ]]; then
            echo "Timed out waiting for daemon (PID $PID) to exit"
            echo "Use --force to send SIGTERM immediately"
            exit 1
        fi
    done
    echo "Daemon (PID $PID) exited after ${elapsed}s"
    rm -f "$PIDFILE"
fi
