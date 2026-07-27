#!/usr/bin/env bash
# loom-daemon-stop.sh - Clean shutdown for the RAW loom-daemon process
# (autonomous work-finder + main-health-gate host — epic #3809, Phase D #3813).
#
# This is NOT the tmux agent pool. `.loom/bin/loom stop` (loom-stop.sh) tears
# down the Manual-Orchestration-Mode tmux pool; THIS script stops the
# `loom-daemon` binary started by loom-daemon-start.sh.
#
# Shutdown model (drain vs. survive):
#   The daemon handles BOTH SIGINT (Ctrl-C) and SIGTERM (`kill <pid>`) by
#   removing its Unix socket and exiting cleanly (#3813). This script sends
#   SIGTERM, waits a grace window, then escalates to SIGKILL if needed.
#
#   In-flight `/loom:sweep` children are NOT cancelled. They are independent
#   detached processes that survive a daemon restart BY DESIGN — stopping the
#   dispatcher must not kill dispatched work. This "survive, don't drain"
#   decision means you can stop+start the daemon without losing running builds;
#   the reaper reconciles their state on the next start. To actively cancel a
#   sweep, use `mcp__loom__cancel_sweep` against a running daemon before stopping.
#
# macOS launchd counterpart (#3972): when loom-daemon-start.sh loaded the
# daemon as a `gui/<uid>` LaunchAgent, this script ALSO unloads the launchd
# job definition (`launchctl bootout`) after the process is confirmed dead —
# not just kills the pid. This matters because the generated plist sets
# RunAtLoad=true (mirroring the validated incident-fix plist), so leaving the
# definition loaded would silently relaunch the daemon at the next login/boot
# even though the operator explicitly stopped it. The pid-kill sequence itself
# is unchanged (SIGTERM -> grace -> SIGKILL, sent directly to the resolved
# pid) — launchd does not auto-respawn on a plain exit (KeepAlive=false), so
# killing the process first is always safe.
#
# Usage:
#   ./.loom/scripts/cli/loom-daemon-stop.sh            Graceful stop (SIGTERM -> SIGKILL)
#   ./.loom/scripts/cli/loom-daemon-stop.sh --force    Skip the grace window (SIGKILL)
#   ./.loom/scripts/cli/loom-daemon-stop.sh --help
#
# Environment:
#   LOOM_DAEMON_STOP_GRACE_SECS   Grace window before SIGKILL (default 10)
#   LOOM_LAUNCHD_LABEL            macOS only: the LaunchAgent label to bootout (default com.rjwalters.loom-daemon)
#
# Exit codes:
#   0  daemon stopped (or was not running)
#   1  usage error / failed to stop

set -uo pipefail

if [[ -t 1 ]]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; NC=''
fi
err()  { echo -e "${RED}$*${NC}" >&2; }
warn() { echo -e "${YELLOW}$*${NC}" >&2; }
ok()   { echo -e "${GREEN}$*${NC}"; }

show_help() {
    # Print the leading comment banner (line 2 through the last comment line
    # before `set -uo pipefail`), stripping the leading "# ". Same pattern as
    # loom-daemon-start.sh's show_help -- robust to the banner growing.
    awk 'NR>=2 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
}

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

FORCE=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h) show_help; exit 0 ;;
        --force|-f) FORCE=true; shift ;;
        *) err "Unknown option '$1'"; echo "Use --help for usage" >&2; exit 1 ;;
    esac
done

REPO_ROOT=$(find_repo_root)
if [[ -z "$REPO_ROOT" ]]; then
    err "Not in a Loom workspace (.loom directory not found)"
    exit 1
fi

PID_FILE="$REPO_ROOT/.loom/.daemon.pid"
GRACE_SECS="${LOOM_DAEMON_STOP_GRACE_SECS:-10}"

# ---------- launchd plumbing (macOS, #3972) ----------
IS_DARWIN=false
[[ "$(uname -s)" == "Darwin" ]] && IS_DARWIN=true
LAUNCHD_LABEL="${LOOM_LAUNCHD_LABEL:-com.rjwalters.loom-daemon}"
LAUNCHD_SERVICE="gui/$(id -u)/${LAUNCHD_LABEL}"

launchd_job_loaded() {
    [[ "$IS_DARWIN" == "true" ]] || return 1
    command -v launchctl >/dev/null 2>&1 || return 1
    launchctl print "$LAUNCHD_SERVICE" >/dev/null 2>&1
}

launchd_job_pid() {
    launchctl print "$LAUNCHD_SERVICE" 2>/dev/null | awk -F'= ' '/^[[:space:]]*pid = /{gsub(/[^0-9]/, "", $2); print $2; exit}'
}

# Unload the launchd job definition so it does NOT silently relaunch at the
# next login/boot (the generated plist sets RunAtLoad=true). Idempotent and
# best-effort -- a job that was never loaded (or already unloaded) is a no-op.
launchd_bootout_if_loaded() {
    if launchd_job_loaded; then
        launchctl bootout "$LAUNCHD_SERVICE" >/dev/null 2>&1 || true
    fi
}

# Resolve the target pid: prefer the PID file, else a launchd lookup (Darwin),
# else best-effort pgrep.
pid=""
if [[ -f "$PID_FILE" ]]; then
    pid=$(cat "$PID_FILE" 2>/dev/null || true)
fi
if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    # PID file missing/stale — try the launchd job (macOS) before falling
    # back to a process-name match.
    if launchd_job_loaded; then
        launchd_pid=$(launchd_job_pid)
        [[ -n "$launchd_pid" ]] && pid="$launchd_pid"
    fi
fi
if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    if command -v pgrep >/dev/null 2>&1; then
        pid=$(pgrep -f '(^|/)loom-daemon$' 2>/dev/null | head -n1 || true)
    fi
fi

if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    warn "No running loom-daemon found (nothing to stop)."
    rm -f "$PID_FILE"
    launchd_bootout_if_loaded
    exit 0
fi

if [[ "$FORCE" == "true" ]]; then
    warn "Force-killing loom-daemon (pid $pid) with SIGKILL..."
    kill -KILL "$pid" 2>/dev/null || true
else
    echo "Stopping loom-daemon (pid $pid) with SIGTERM (grace ${GRACE_SECS}s)..."
    kill -TERM "$pid" 2>/dev/null || true
    # Wait up to the grace window for a clean exit.
    waited=0
    while kill -0 "$pid" 2>/dev/null && (( waited < GRACE_SECS )); do
        sleep 1
        waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        warn "Daemon did not exit within ${GRACE_SECS}s — escalating to SIGKILL."
        kill -KILL "$pid" 2>/dev/null || true
        sleep 1
    fi
fi

if kill -0 "$pid" 2>/dev/null; then
    err "Failed to stop loom-daemon (pid $pid)."
    exit 1
fi

# Unload the launchd job definition (macOS, #3972) now that the process is
# confirmed dead -- RunAtLoad=true on the generated plist means leaving it
# loaded would silently relaunch the daemon at the next login/boot.
launchd_bootout_if_loaded

rm -f "$PID_FILE"
ok "loom-daemon stopped (pid $pid)."
echo "In-flight sweeps (if any) were left running by design; the next start reconciles them."
exit 0
