#!/usr/bin/env bash
# Start the Loom daemon as a background process.
#
# The daemon starts independently of any Claude Code session.  When invoked
# from the shell (not from within Claude Code), its process tree is:
#
#   init/launchd → loom-daemon → loom-shepherd.sh → claude /builder
#
# This means worker claude sessions are NOT descendants of any Claude Code
# session, which avoids the nested-Claude-Code spawning restrictions that
# cause shepherd spawning failures when /loom runs the daemon directly.
#
# Usage:
#   ./.loom/scripts/start-daemon.sh [OPTIONS]
#
# Options:
#   --force, -f         Enable force mode (auto-promote proposals, auto-merge)
#   --merge, -m         Alias for --force
#   --timeout-min N     Stop daemon after N minutes (0 = no timeout)
#   --debug, -d         Enable debug logging
#   --status            Print daemon status and exit
#   --stop              Write stop signal and exit
#   --help, -h          Show this help
#
# After starting, the /loom Claude Code skill detects the running daemon
# via .loom/daemon-loop.pid and operates as a signal-writer + observer.
# Write spawn_shepherd / stop / etc. signals to .loom/signals/ to control
# the daemon from within Claude Code without spawning subprocesses.
#
# Stopping the daemon:
#   touch .loom/stop-daemon         # Graceful shutdown (via file signal)
#   ./.loom/scripts/stop-daemon.sh  # Convenience wrapper (same effect)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PIDFILE="$REPO_ROOT/.loom/daemon-loop.pid"
LOGFILE="$REPO_ROOT/.loom/daemon.log"
STOP_SIGNAL="$REPO_ROOT/.loom/stop-daemon"

# ── Argument parsing ──────────────────────────────────────────────────────────
ARGS=()
STATUS_ONLY=false
STOP_ONLY=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --status)        STATUS_ONLY=true;;
        --stop)          STOP_ONLY=true;;
        --help|-h)
            sed -n '/^# Usage:/,/^[^#]/p' "$0" | head -n -1 | sed 's/^# \?//'
            exit 0
            ;;
        *)               ARGS+=("$1");;
    esac
    shift
done

# ── Status check ──────────────────────────────────────────────────────────────
if "$STATUS_ONLY"; then
    if [[ -f "$PIDFILE" ]]; then
        PID="$(cat "$PIDFILE")"
        if kill -0 "$PID" 2>/dev/null; then
            if command -v loom-status >/dev/null 2>&1; then
                loom-status --fast
                exit 0
            fi
            echo "Daemon running (PID $PID)"
            exit 0
        else
            echo "Daemon not running (stale PID file)"
            rm -f "$PIDFILE"
            exit 1
        fi
    else
        if command -v loom-status >/dev/null 2>&1; then
            loom-status --fast
            exit 1
        fi
        echo "Daemon not running"
        exit 1
    fi
fi

# ── Stop request ──────────────────────────────────────────────────────────────
if "$STOP_ONLY"; then
    touch "$STOP_SIGNAL"
    echo "Stop signal written to $STOP_SIGNAL"
    echo "The daemon will exit after completing its current iteration."
    exit 0
fi

# ── Already running? ──────────────────────────────────────────────────────────
if [[ -f "$PIDFILE" ]]; then
    PID="$(cat "$PIDFILE")"
    if kill -0 "$PID" 2>/dev/null; then
        echo "Daemon already running (PID $PID)"
        echo "Use --status to check, --stop to shut down."
        exit 0
    else
        echo "Removing stale PID file (PID $PID was not running)"
        rm -f "$PIDFILE"
    fi
fi

# ── Ensure required directories exist ────────────────────────────────────────
mkdir -p "$REPO_ROOT/.loom/logs" \
         "$REPO_ROOT/.loom/signals" \
         "$REPO_ROOT/.loom/progress"

# Remove any leftover stop signal from a previous session
rm -f "$STOP_SIGNAL"

# ── Locate loom-daemon executable ────────────────────────────────────────────
# Prefer the installed loom-daemon CLI; fall back to running the Python module.
LOOM_DAEMON_CMD=""

if command -v loom-daemon &>/dev/null; then
    LOOM_DAEMON_CMD="loom-daemon"
else
    # Try loom-daemon.sh wrapper (which handles PYTHONPATH setup)
    DAEMON_SH="$SCRIPT_DIR/loom-daemon.sh"
    if [[ -x "$DAEMON_SH" ]]; then
        LOOM_DAEMON_CMD="$DAEMON_SH"
    else
        # Last resort: invoke Python module directly
        LOOM_TOOLS_SRC="$REPO_ROOT/loom-tools/src"
        if [[ -d "$LOOM_TOOLS_SRC" ]]; then
            LOOM_DAEMON_CMD="env PYTHONPATH=$LOOM_TOOLS_SRC:${PYTHONPATH:-} python3 -m loom_tools.daemon_v2.cli"
        else
            echo "ERROR: Cannot locate loom-daemon executable." >&2
            echo "Run: pip install -e $REPO_ROOT/loom-tools" >&2
            exit 1
        fi
    fi
fi

# ── Launch daemon with nohup ──────────────────────────────────────────────────
# nohup ensures the daemon survives when the calling terminal/Claude Code
# session exits.  The process tree becomes:
#   init/launchd → (nohup) loom-daemon → children
#
# Output is appended to the daemon log file.
echo "Starting Loom daemon..."
echo "  Log:  $LOGFILE"
echo "  PID:  $PIDFILE"
echo "  Mode: ${ARGS[*]:-normal}"

# Append a startup marker to the log
{
    echo ""
    echo "========================================"
    echo " start-daemon.sh: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo " Args: ${ARGS[*]:-<none>}"
    echo "========================================"
} >> "$LOGFILE" 2>/dev/null || true

# shellcheck disable=SC2086
nohup $LOOM_DAEMON_CMD "${ARGS[@]+"${ARGS[@]}"}" \
    >> "$LOGFILE" 2>&1 &
DAEMON_PID=$!

echo "Daemon started (PID $DAEMON_PID)"
echo ""
echo "Monitor:  tail -f $LOGFILE"
echo "Status:   $SCRIPT_DIR/start-daemon.sh --status"
echo "Stop:     $SCRIPT_DIR/stop-daemon.sh"
echo ""
echo "Start /loom in Claude Code to begin orchestration:"
echo "  /loom"
echo "  /loom --merge   # force mode: auto-promote + auto-merge"
