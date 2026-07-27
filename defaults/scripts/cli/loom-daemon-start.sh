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
#   - starts a plain reliability daemon with BOTH autonomous loops OFF by
#     default (matching the ecosystem-wide opt-in / default-off contract:
#     LOOM_WORK_FINDER unset => off, LOOM_MAIN_HEALTH_GATE unset => off). Opt in
#     explicitly with --work-finder / --health-gate, or hand control to
#     .loom/config.json -> autonomous with --from-config (#3911),
#   - on macOS, backgrounds the daemon as a `gui/<uid>` LaunchAgent (#3972) so
#     it survives the launching session's death instead of a plain `nohup ...
#     &`; on Linux it stays a plain nohup background job,
#   - backgrounds the daemon and writes a PID file (.loom/.daemon.pid),
#   - persists the resolved invocation flags to .loom/.daemon.flags so
#     `loom-daemon-update.sh` (#3968) can restart with EXACTLY the same
#     autonomy flags after a rebuild — never wider,
#   - surfaces the singleton-guard refusal (#3806) legibly instead of leaving a
#     silently-exited background process.
#
# Default is FLAGS-OFF: a bare `loom-daemon-start.sh` does NOT auto-dispatch
# sweeps. This is a deliberate safe default — enable autonomy explicitly.
#
# macOS session-bootstrap hazard (#3972): a plain `nohup "$DAEMON_BIN" &`
# leaves the process wired into the LAUNCHING SESSION's Mach bootstrap
# namespace. When that session dies (a Claude Code session crash, a closed
# terminal, a dropped SSH connection) the daemon and every child it spawns
# start failing XPC lookups to trustd (cert verification -- `gh` TLS errors)
# and opendirectoryd (`getpwuid` -- "No user exists for uid N" from `git`),
# with NO crash and no obvious log signal beyond those downstream errors. This
# is why "start it from a terminal that might die" is unsafe on macOS. This
# script defaults to loading the daemon as a `launchd` LaunchAgent on Darwin
# specifically to avoid that failure mode; see --no-launchd below for the
# escape hatch and daemon-reference.md Operability for the incident writeup.
#
# Usage:
#   ./.loom/scripts/cli/loom-daemon-start.sh                 Reliability daemon (both loops OFF)
#   ./.loom/scripts/cli/loom-daemon-start.sh --work-finder   Enable the autonomous work finder
#   ./.loom/scripts/cli/loom-daemon-start.sh --health-gate   Enable the main-health gate
#   ./.loom/scripts/cli/loom-daemon-start.sh --work-finder --health-gate   Both loops ON
#   ./.loom/scripts/cli/loom-daemon-start.sh --from-config   Enable per .loom/config.json only
#   ./.loom/scripts/cli/loom-daemon-start.sh --no-work-finder    Force work finder OFF (explicit)
#   ./.loom/scripts/cli/loom-daemon-start.sh --no-health-gate    Force health gate OFF (explicit)
#   ./.loom/scripts/cli/loom-daemon-start.sh --foreground    Run in the foreground (no PID file)
#   ./.loom/scripts/cli/loom-daemon-start.sh --no-launchd    macOS only: use legacy nohup instead of a LaunchAgent
#   ./.loom/scripts/cli/loom-daemon-start.sh --print-plist   Print the LaunchAgent plist that WOULD be installed and exit (no side effects)
#   ./.loom/scripts/cli/loom-daemon-start.sh --help
#
# Environment:
#   LOOM_DAEMON_BIN     Path to the loom-daemon binary (else auto-detected)
#   LOOM_SOCKET_PATH    Override the daemon socket (default ~/.loom/loom-daemon.sock)
#   LOOM_WORK_FINDER / LOOM_MAIN_HEALTH_GATE  Respected when already exported
#   LOOM_DAEMON_LAUNCHD  macOS only: 0/false/no forces the legacy nohup path (same as --no-launchd)
#   LOOM_LAUNCHD_LABEL   macOS only: override the LaunchAgent label (default com.rjwalters.loom-daemon)
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
    # Print the leading comment banner (line 2 through the last comment line
    # before `set -uo pipefail`), stripping the leading "# ".
    awk 'NR>=2 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
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

# ---------- launchd plist rendering (#3972) ----------
# Pure string rendering -- safe to call on ANY platform (used by
# --print-plist for inspection/testing). The actual `launchctl` invocation
# that consumes this plist is gated to Darwin separately, below.
xml_escape() {
    local s="$1"
    s="${s//&/&amp;}"
    s="${s//</&lt;}"
    s="${s//>/&gt;}"
    printf '%s' "$s"
}

resolve_launchd_label() {
    echo "${LOOM_LAUNCHD_LABEL:-com.rjwalters.loom-daemon}"
}

# render_launchd_plist <label> <daemon_bin> <workdir> <log_path>
# Prints the LaunchAgent plist XML to stdout. Mirrors the hand-written plist
# that validated the #3972 fix during the incident
# (~/Library/LaunchAgents/com.rjwalters.loom-daemon.plist): RunAtLoad=true
# (the daemon also comes back after a reboot/re-login, not just a session
# death -- strictly more durable than the pre-#3972 nohup contract, which
# didn't survive a reboot either) and KeepAlive=false (launchd does not
# auto-respawn a crashed daemon; that stays the reaper/operator's job, same as
# before). Lifecycle (first start / explicit stop) is still entirely
# operator-driven via loom-daemon-start.sh / loom-daemon-stop.sh -- bootout on
# stop unloads the definition so it does NOT come back at the next login. The
# PATH is the CURRENT PATH plus a fallback set (~/.local/bin, ~/.cargo/bin,
# Homebrew, standard bin dirs) so `gh`, `git`, `cargo`, and `python3` resolve
# inside the LaunchAgent's minimal launchd environment even if the interactive
# shell's PATH customizations aren't present there. Every already-exported
# LOOM_* / GH_TOKEN / GITEA_TOKEN / FORGE_TOKEN var is forwarded verbatim so
# the launchd job sees EXACTLY the autonomy flags and auth this invocation
# resolved -- never wider, never narrower (#3972 AC: "preserves the current
# flag semantics").
render_launchd_plist() {
    local label="$1" bin="$2" workdir="$3" log_path="$4"
    local plist_path_value="${PATH}:${HOME}/.local/bin:${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

    local env_entries=""
    env_entries+="        <key>PATH</key>\n        <string>$(xml_escape "$plist_path_value")</string>\n"
    env_entries+="        <key>HOME</key>\n        <string>$(xml_escape "$HOME")</string>\n"

    local line key value
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        key="${line%%=*}"
        value="${line#*=}"
        env_entries+="        <key>$(xml_escape "$key")</key>\n        <string>$(xml_escape "$value")</string>\n"
    done < <(env | grep -E '^(LOOM_[A-Za-z0-9_]*|GH_TOKEN|GITEA_TOKEN|FORGE_TOKEN)=' || true)

    printf '<?xml version="1.0" encoding="UTF-8"?>\n'
    printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    printf '<plist version="1.0">\n<dict>\n'
    printf '    <key>Label</key>\n    <string>%s</string>\n' "$(xml_escape "$label")"
    printf '    <key>ProgramArguments</key>\n    <array>\n        <string>%s</string>\n    </array>\n' "$(xml_escape "$bin")"
    printf '    <key>WorkingDirectory</key>\n    <string>%s</string>\n' "$(xml_escape "$workdir")"
    printf '    <key>EnvironmentVariables</key>\n    <dict>\n'
    printf '%b' "$env_entries"
    printf '    </dict>\n'
    printf '    <key>RunAtLoad</key>\n    <true/>\n'
    printf '    <key>KeepAlive</key>\n    <false/>\n'
    printf '    <key>ProcessType</key>\n    <string>Background</string>\n'
    printf '    <key>StandardOutPath</key>\n    <string>%s</string>\n' "$(xml_escape "$log_path")"
    printf '    <key>StandardErrorPath</key>\n    <string>%s</string>\n' "$(xml_escape "$log_path")"
    printf '</dict>\n</plist>\n'
}

# ---------- args ----------
# Capture the raw invocation args before the parsing loop consumes "$@" — used
# below to persist exactly what was passed (Issue #3968: `loom-daemon-update.sh`
# replays these flags verbatim on restart, so a rebuild+restart never widens the
# FLAGS-OFF/opt-in contract).
ORIGINAL_ARGS=("$@")

# Default is FLAGS-OFF (#3911): both autonomous loops default OFF, matching the
# ecosystem-wide opt-in / default-off contract. Opt in with --work-finder /
# --health-gate, or hand control to config with --from-config.
FROM_CONFIG=false
FOREGROUND=false
WANT_WORK_FINDER=false
WANT_HEALTH_GATE=false
NO_LAUNCHD=false
PRINT_PLIST=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h) show_help; exit 0 ;;
        --from-config) FROM_CONFIG=true; shift ;;
        --foreground|--fg) FOREGROUND=true; shift ;;
        --work-finder) WANT_WORK_FINDER=true; shift ;;
        --health-gate) WANT_HEALTH_GATE=true; shift ;;
        --no-work-finder) WANT_WORK_FINDER=false; shift ;;
        --no-health-gate) WANT_HEALTH_GATE=false; shift ;;
        --no-launchd) NO_LAUNCHD=true; shift ;;
        --print-plist) PRINT_PLIST=true; shift ;;
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
# default is FLAGS-OFF (#3911) — a plain start is a reliability daemon with both
# autonomous loops OFF, matching the ecosystem-wide opt-in / default-off contract
# (LOOM_WORK_FINDER unset => off, LOOM_MAIN_HEALTH_GATE unset => off). Opt in with
# --work-finder / --health-gate (force the var to 1), or pass --from-config to
# leave both unset so .loom/config.json -> autonomous drives.
export LOOM_WORKSPACE="${LOOM_WORKSPACE:-$REPO_ROOT}"

# ---------- guard-hook autonomy defaults (#3898) ----------
# The daemon dispatches headless /loom:sweep children under
# --dangerously-skip-permissions, where a guard ASK has no human to answer it
# and therefore BLOCKS — a silent stall. So autonomous runs get two guard
# defaults, both env-overridable (an already-exported value always wins):
#   * LOOM_GUARD_DECISION_LOG=1 — capture every guard DENY/ASK to
#     .loom/logs/guard-decisions.log so the standing per-trigger review policy
#     (see CLAUDE.md → "Autonomous guard defaults") can dedup by pattern and
#     file one issue per distinct trigger. Off by default outside autonomous
#     mode; here we opt it on so the feedback loop actually has data.
#   * LOOM_FORCE_SCOPE=protected — allow an agent to force-push / hard-reset its
#     OWN working branch without a stall, while force-push to a protected branch
#     (main/master/default) stays a hard DENY via ALWAYS_BLOCK_PATTERNS. This is
#     the Loom-recommended force-scope for autonomous repos.
# Children inherit these through the daemon's process environment.
export LOOM_GUARD_DECISION_LOG="${LOOM_GUARD_DECISION_LOG:-1}"
export LOOM_FORCE_SCOPE="${LOOM_FORCE_SCOPE:-protected}"

if [[ "$FROM_CONFIG" == "true" ]]; then
    echo -e "${BOLD}Autonomous mode: driven by .loom/config.json -> autonomous (env not forced)${NC}"
else
    # An already-exported env var always wins. Otherwise --work-finder /
    # --health-gate force the loop ON (=1); the default (flags off) forces it
    # OFF (=0), so a plain start is a reliability daemon that never auto-dispatches.
    if [[ "$WANT_WORK_FINDER" == "true" ]]; then
        export LOOM_WORK_FINDER="${LOOM_WORK_FINDER:-1}"
    else
        export LOOM_WORK_FINDER="${LOOM_WORK_FINDER:-0}"
    fi
    if [[ "$WANT_HEALTH_GATE" == "true" ]]; then
        export LOOM_MAIN_HEALTH_GATE="${LOOM_MAIN_HEALTH_GATE:-1}"
    else
        export LOOM_MAIN_HEALTH_GATE="${LOOM_MAIN_HEALTH_GATE:-0}"
    fi
    if [[ "$LOOM_WORK_FINDER" == "0" && "$LOOM_MAIN_HEALTH_GATE" == "0" ]]; then
        echo -e "${BOLD}Reliability daemon:${NC} work_finder=off main_health_gate=off (both loops OFF; opt in with --work-finder / --health-gate / --from-config)"
    else
        echo -e "${BOLD}Autonomous mode:${NC} work_finder=${LOOM_WORK_FINDER} main_health_gate=${LOOM_MAIN_HEALTH_GATE}"
    fi
fi

# ---------- persist invocation flags (Issue #3968) ----------
# `loom-daemon-update.sh` reads this file to restart with EXACTLY the same
# autonomy flags after a rebuild — the FLAGS-OFF/opt-in contract must never
# widen across an update. Script-only flags that don't describe daemon
# autonomy state (--foreground/--fg, --help/-h) are filtered out; everything
# else (--from-config, --work-finder, --health-gate, --no-work-finder,
# --no-health-gate) is preserved verbatim, one per line. Written on every
# start attempt (success or failure) so the record always reflects the most
# recent invocation.
FLAGS_FILE="$REPO_ROOT/.loom/.daemon.flags"
: > "$FLAGS_FILE"
# Guard the array expansion: a bare invocation (the common case) leaves
# ORIGINAL_ARGS empty, and "${arr[@]}" on a zero-element array is an unbound
# variable error under `set -u` on bash < 4.4 (still the default /bin/bash on
# stock macOS). ${#ORIGINAL_ARGS[@]} is always safe to query.
if [[ "${#ORIGINAL_ARGS[@]}" -gt 0 ]]; then
    for _flag_arg in "${ORIGINAL_ARGS[@]}"; do
        case "$_flag_arg" in
            --foreground|--fg|--help|-h|--no-launchd|--print-plist) continue ;;
            *) echo "$_flag_arg" >> "$FLAGS_FILE" ;;
        esac
    done
    unset _flag_arg
fi

echo "Daemon binary: $DAEMON_BIN"
echo "Socket:        $SOCKET_PATH"
echo "Daemon log:    ${HOME}/.loom/daemon.log"

# ---------- foreground mode ----------
if [[ "$FOREGROUND" == "true" ]]; then
    echo "Starting loom-daemon in the foreground (Ctrl-C to stop)..."
    exec "$DAEMON_BIN"
fi

# ---------- platform detection (#3972) ----------
IS_DARWIN=false
[[ "$(uname -s)" == "Darwin" ]] && IS_DARWIN=true

USE_LAUNCHD=false
if [[ "$IS_DARWIN" == "true" ]]; then
    USE_LAUNCHD=true
    if [[ "${LOOM_DAEMON_LAUNCHD:-}" =~ ^(0|false|no)$ ]]; then
        USE_LAUNCHD=false
    fi
fi
[[ "$NO_LAUNCHD" == "true" ]] && USE_LAUNCHD=false

# ---------- --print-plist: pure inspection, no side effects ----------
if [[ "$PRINT_PLIST" == "true" ]]; then
    render_launchd_plist "$(resolve_launchd_label)" "$DAEMON_BIN" "$REPO_ROOT" "$START_LOG"
    exit 0
fi

# ---------- background + PID file ----------
: > "$START_LOG"

if [[ "$USE_LAUNCHD" == "true" ]] && ! command -v launchctl >/dev/null 2>&1; then
    warn "launchctl not found despite running on Darwin -- falling back to nohup."
    USE_LAUNCHD=false
fi

if [[ "$USE_LAUNCHD" == "true" ]]; then
    # ---------- macOS: launchd LaunchAgent (#3972) ----------
    # A plain `nohup ... &` stays in the LAUNCHING SESSION's Mach bootstrap
    # namespace; when that session dies, trustd/opendirectoryd XPC lookups
    # start failing for the daemon and every child it spawns (gh TLS errors,
    # "No user exists for uid N" from git) with no crash and no obvious log
    # signal. Loading as a `gui/<uid>` LaunchAgent keeps the daemon in the
    # user's durable GUI bootstrap domain instead, independent of whichever
    # terminal/session launched it. See daemon-reference.md Operability for
    # the incident writeup. Escape hatch: --no-launchd / LOOM_DAEMON_LAUNCHD=0.
    LAUNCHD_LABEL=$(resolve_launchd_label)
    LAUNCHD_UID=$(id -u)
    LAUNCHD_DOMAIN="gui/${LAUNCHD_UID}"
    LAUNCHD_SERVICE="${LAUNCHD_DOMAIN}/${LAUNCHD_LABEL}"
    PLIST_DIR="$HOME/Library/LaunchAgents"
    PLIST_FILE="$PLIST_DIR/${LAUNCHD_LABEL}.plist"
    mkdir -p "$PLIST_DIR"

    render_launchd_plist "$LAUNCHD_LABEL" "$DAEMON_BIN" "$REPO_ROOT" "$START_LOG" > "$PLIST_FILE"

    echo "Launchd label:  $LAUNCHD_LABEL"
    echo "Launchd plist:  $PLIST_FILE"

    # Reload with the freshly-rendered plist every time -- a job left loaded
    # from a prior invocation (possibly with different flags/env) must not
    # silently keep running its OLD definition.
    if launchctl print "$LAUNCHD_SERVICE" >/dev/null 2>&1; then
        launchctl bootout "$LAUNCHD_SERVICE" >/dev/null 2>&1 || true
    fi

    BOOTSTRAP_ERR="$START_LOG.bootstrap-err"
    if ! launchctl bootstrap "$LAUNCHD_DOMAIN" "$PLIST_FILE" 2>"$BOOTSTRAP_ERR"; then
        err "launchctl bootstrap failed for $LAUNCHD_SERVICE:"
        cat "$BOOTSTRAP_ERR" >&2 2>/dev/null || true
        rm -f "$BOOTSTRAP_ERR"
        exit 1
    fi
    rm -f "$BOOTSTRAP_ERR"

    # RunAtLoad=true means bootstrap alone would already start it, but we
    # kickstart -k explicitly anyway so THIS invocation deterministically wins
    # (the -k kill-first semantics guarantee a fresh process picking up the
    # plist we just wrote, rather than racing launchd's own RunAtLoad timing).
    KICKSTART_ERR="$START_LOG.kickstart-err"
    if ! launchctl kickstart -k "$LAUNCHD_SERVICE" 2>"$KICKSTART_ERR"; then
        err "launchctl kickstart failed for $LAUNCHD_SERVICE:"
        cat "$KICKSTART_ERR" >&2 2>/dev/null || true
        rm -f "$KICKSTART_ERR"
        exit 1
    fi
    rm -f "$KICKSTART_ERR"

    # Give it a moment to either bind the socket or trip the singleton guard.
    sleep 2

    daemon_pid=$(launchctl print "$LAUNCHD_SERVICE" 2>/dev/null | awk -F'= ' '/^[[:space:]]*pid = /{gsub(/[^0-9]/, "", $2); print $2; exit}')

    if [[ -z "$daemon_pid" ]] || ! kill -0 "$daemon_pid" 2>/dev/null; then
        err "loom-daemon did not stay running under launchd ($LAUNCHD_SERVICE)."
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
    ok "loom-daemon started under launchd (pid $daemon_pid, label $LAUNCHD_LABEL)."
    echo "PID file: $PID_FILE"
    echo "Stop with: ./.loom/scripts/cli/loom-daemon-stop.sh"
    exit 0
fi

# ---------- Linux (or --no-launchd): plain nohup background job ----------
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
