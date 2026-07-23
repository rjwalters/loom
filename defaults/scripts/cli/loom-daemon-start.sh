#!/usr/bin/env bash
# loom-daemon-start.sh - Safe start wrapper for the RAW loom-daemon process
# (the autonomous work-finder + main-health-gate host — epic #3809, Phase D
# #3813).
#
# This is NOT the tmux agent pool. `.loom/bin/loom start` (loom-start.sh)
# manages the Manual-Orchestration-Mode tmux pool; THIS script backgrounds the
# `loom-daemon` binary itself, which hosts the autonomous forge-polling work
# finder (#3810) and the reactive main-health gate (#3812). The two process
# models are independent and can coexist.
#
# It:
#   - locates the loom-daemon binary,
#   - runs the (advisory, never-blocking) host-sleep check (#3350),
#   - enables autonomous mode (work finder + health gate) via env by default,
#     or leaves it to .loom/config.json -> autonomous with --from-config,
#   - backgrounds the daemon and writes a PID file (.loom/.daemon.pid),
#   - surfaces the singleton-guard refusal (#3806) legibly instead of leaving a
#     silently-exited background process.
#
# Usage:
#   ./.loom/scripts/cli/loom-daemon-start.sh                 Start autonomous mode
#   ./.loom/scripts/cli/loom-daemon-start.sh --from-config   Enable per .loom/config.json only
#   ./.loom/scripts/cli/loom-daemon-start.sh --no-work-finder    Health gate only
#   ./.loom/scripts/cli/loom-daemon-start.sh --no-health-gate    Work finder only
#   ./.loom/scripts/cli/loom-daemon-start.sh --foreground    Run in the foreground (no PID file)
#   ./.loom/scripts/cli/loom-daemon-start.sh --help
#
# Environment:
#   LOOM_DAEMON_BIN     Path to the loom-daemon binary (else auto-detected)
#   LOOM_SOCKET_PATH    Override the daemon socket (default ~/.loom/loom-daemon.sock)
#   LOOM_WORK_FINDER / LOOM_MAIN_HEALTH_GATE  Respected when already exported
#
# Exit codes:
#   0  daemon started (or already running)
#   1  usage error / binary not found / daemon failed to start

set -uo pipefail

# ---------- output helpers ----------
if [[ -t 1 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BOLD='\033[1m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; NC=''
fi
err()  { echo -e "${RED}$*${NC}" >&2; }
warn() { echo -e "${YELLOW}$*${NC}" >&2; }
ok()   { echo -e "${GREEN}$*${NC}"; }

show_help() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------- repo root ----------
find_repo_root() {
    local dir="$PWD"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.loom" ]]; then echo "$dir"; return 0; fi
        if [[ -f "$dir/.git" ]]; then
            local gitdir main_repo
            gitdir=$(sed 's/^gitdir: //' "$dir/.git")
            main_repo=$(dirname "$(dirname "$(dirname "$gitdir")")")
            if [[ -d "$main_repo/.loom" ]]; then echo "$main_repo"; return 0; fi
        fi
        dir="$(dirname "$dir")"
    done
    echo ""
}

# ---------- locate the daemon binary ----------
locate_daemon_bin() {
    local root="$1"
    if [[ -n "${LOOM_DAEMON_BIN:-}" && -x "${LOOM_DAEMON_BIN}" ]]; then
        echo "${LOOM_DAEMON_BIN}"; return 0
    fi
    if command -v loom-daemon >/dev/null 2>&1; then
        command -v loom-daemon; return 0
    fi
    local candidate
    for candidate in \
        "$root/loom-daemon/target/release/loom-daemon" \
        "$root/loom-daemon/target/debug/loom-daemon" \
        "$root/target/release/loom-daemon" \
        "$root/target/debug/loom-daemon"; do
        if [[ -x "$candidate" ]]; then echo "$candidate"; return 0; fi
    done
    echo ""
}

# ---------- args ----------
FROM_CONFIG=false
FOREGROUND=false
WANT_WORK_FINDER=true
WANT_HEALTH_GATE=true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h) show_help; exit 0 ;;
        --from-config) FROM_CONFIG=true; shift ;;
        --foreground|--fg) FOREGROUND=true; shift ;;
        --no-work-finder) WANT_WORK_FINDER=false; shift ;;
        --no-health-gate) WANT_HEALTH_GATE=false; shift ;;
        *) err "Unknown option '$1'"; echo "Use --help for usage" >&2; exit 1 ;;
    esac
done

REPO_ROOT=$(find_repo_root)
if [[ -z "$REPO_ROOT" ]]; then
    err "Not in a Loom workspace (.loom directory not found)"
    exit 1
fi

DAEMON_BIN=$(locate_daemon_bin "$REPO_ROOT")
if [[ -z "$DAEMON_BIN" ]]; then
    err "loom-daemon binary not found."
    echo "Build it (cargo build --release -p loom-daemon) or set LOOM_DAEMON_BIN=/path/to/loom-daemon" >&2
    exit 1
fi

PID_FILE="$REPO_ROOT/.loom/.daemon.pid"
SOCKET_PATH="${LOOM_SOCKET_PATH:-$HOME/.loom/loom-daemon.sock}"
START_LOG="$REPO_ROOT/.loom/logs/daemon-start.log"
mkdir -p "$REPO_ROOT/.loom/logs"

# ---------- already-running guard (PID file) ----------
if [[ -f "$PID_FILE" ]]; then
    existing_pid=$(cat "$PID_FILE" 2>/dev/null || true)
    if [[ -n "$existing_pid" ]] && kill -0 "$existing_pid" 2>/dev/null; then
        warn "loom-daemon already running (pid $existing_pid, per $PID_FILE)."
        echo "To restart: ./.loom/scripts/cli/loom-daemon-stop.sh && $0" >&2
        exit 0
    fi
    # Stale PID file — clean it up and continue.
    rm -f "$PID_FILE"
fi

# ---------- advisory host-sleep check (never blocks — #3350) ----------
SLEEP_CHECK="$REPO_ROOT/.loom/scripts/check-host-sleep.sh"
[[ -x "$SLEEP_CHECK" ]] || SLEEP_CHECK="$REPO_ROOT/defaults/scripts/check-host-sleep.sh"
if [[ -x "$SLEEP_CHECK" ]]; then
    "$SLEEP_CHECK" || true
fi

# ---------- autonomous-mode env ----------
# Precedence: an already-exported env var is always respected. Otherwise the
# default is to enable both loops (the whole point of starting the daemon),
# unless --from-config was passed (then leave them unset so .loom/config.json ->
# autonomous drives) or a --no-* opt-out forces the var to 0.
export LOOM_WORKSPACE="${LOOM_WORKSPACE:-$REPO_ROOT}"

if [[ "$FROM_CONFIG" == "true" ]]; then
    echo -e "${BOLD}Autonomous mode: driven by .loom/config.json -> autonomous (env not forced)${NC}"
else
    if [[ "$WANT_WORK_FINDER" == "false" ]]; then
        export LOOM_WORK_FINDER=0
    elif [[ -z "${LOOM_WORK_FINDER:-}" ]]; then
        export LOOM_WORK_FINDER=1
    fi
    if [[ "$WANT_HEALTH_GATE" == "false" ]]; then
        export LOOM_MAIN_HEALTH_GATE=0
    elif [[ -z "${LOOM_MAIN_HEALTH_GATE:-}" ]]; then
        export LOOM_MAIN_HEALTH_GATE=1
    fi
    echo -e "${BOLD}Autonomous mode:${NC} work_finder=${LOOM_WORK_FINDER:-config} main_health_gate=${LOOM_MAIN_HEALTH_GATE:-config}"
fi

echo "Daemon binary: $DAEMON_BIN"
echo "Socket:        $SOCKET_PATH"
echo "Daemon log:    ${HOME}/.loom/daemon.log"

# ---------- foreground mode ----------
if [[ "$FOREGROUND" == "true" ]]; then
    echo "Starting loom-daemon in the foreground (Ctrl-C to stop)..."
    exec "$DAEMON_BIN"
fi

# ---------- background + PID file ----------
: > "$START_LOG"
nohup "$DAEMON_BIN" >> "$START_LOG" 2>&1 &
daemon_pid=$!

# Give it a moment to either bind the socket or trip the singleton guard.
sleep 2

if ! kill -0 "$daemon_pid" 2>/dev/null; then
    err "loom-daemon exited immediately after start (pid $daemon_pid)."
    if [[ -s "$START_LOG" ]]; then
        echo "----- startup output ($START_LOG) -----" >&2
        tail -n 20 "$START_LOG" >&2
        echo "---------------------------------------" >&2
    fi
    warn "If another daemon is already listening on the socket, stop it first"
    warn "(./.loom/scripts/cli/loom-daemon-stop.sh) and retry."
    exit 1
fi

echo "$daemon_pid" > "$PID_FILE"
ok "loom-daemon started (pid $daemon_pid). PID file: $PID_FILE"
echo "Stop with: ./.loom/scripts/cli/loom-daemon-stop.sh"
exit 0
