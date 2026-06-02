#!/usr/bin/env bash
# spawn-loop.sh - Minimal multi-account spawn loop for /loom:sweep (Phase 1, #3374).
#
# Phase 1 of the shepherd/daemon deprecation epic (#3372). Replaces the
# ~4,200 LOC Python daemon brain (`daemon_v2/`) with a ~200-LOC bash poller
# that does only the load-bearing thing: launch one `claude -p "/loom:sweep N"`
# per ready issue, with token rotation via spawn-claude.sh.
#
# What this script does:
#   1. Polls `gh issue list --label loom:issue --state open --limit 50`.
#   2. While `len(running) < MAX_PARALLEL`, atomically claims the next ready
#      issue (label flip + mkdir-based file lock under `.loom/locks/issue-<N>/`)
#      and detaches `spawn-claude.sh -p "/loom:sweep <N>"`. Each spawned process
#      picks its own token from `.loom/tokens/.ranking`.
#   3. Sleeps POLL_INTERVAL seconds, then reaps exited children.
#   4. If a child dies but the issue is still `loom:building` AND a sweep
#      checkpoint exists at `.loom/sweep-checkpoint/issue-<N>.json`, the next
#      tick re-spawns. The sweep skill itself reads the checkpoint on entry
#      and skips completed phases (--resume semantics shipped in #3373).
#
# What this script does NOT do (Phase 2 territory):
#   - Work generation triggers (Architect/Hermit/Auditor cadence)
#   - Periodic support roles (Champion, Guide, Curator)
#   - Shepherd-N pool slot bookkeeping (we track a flat list of children)
#   - Cross-session retry history or `last_*_trigger` cooldowns
#   - Tauri / MCP integration
#
# Coexistence with daemon_v2:
#   If `.loom/daemon-loop.pid` exists and the process is alive, we warn at
#   startup but proceed. Both will try to claim `loom:issue` items; the
#   label flip + lock file race resolves cleanly (last writer wins on
#   `loom:building`, first mkdir wins on the lock). Operators are expected
#   to pick one or the other, not run both.
#
# Opt-in only:
#   Refuses to run unless `LOOM_USE_SPAWN_LOOP=1` is exported. This protects
#   existing daemon users from accidentally starting a parallel orchestrator.
#
# Usage:
#   LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start
#   ./.loom/scripts/spawn-loop.sh stop                 # or: touch .loom/stop-spawn-loop
#   ./.loom/scripts/spawn-loop.sh status
#
# Environment overrides:
#   LOOM_USE_SPAWN_LOOP    Must be `1` to start. (mandatory)
#   MAX_PARALLEL           Concurrent children (default: 3)
#   POLL_INTERVAL          Seconds between ticks (default: 30)
#   SHUTDOWN_GRACE_SEC     Wait this long for children on stop (default: 300)
#   LOOM_REPO              Override repo for `gh issue list` (default: from gh remote)
#
# State files:
#   .loom/spawn-loop.pid             Running PID
#   .loom/spawn-loop-state.json      {started_at, running:[{issue, pid, started_at, token}]}
#   .loom/logs/spawn-loop.log        Loop log (timestamped spawn/exit/error entries)
#   .loom/logs/sweep-issue-<N>.log   Per-issue child output
#   .loom/locks/issue-<N>/           Atomic claim lock (mkdir primitive)
#   .loom/stop-spawn-loop            Touch to request graceful shutdown
#
# Exit codes:
#   0   Normal exit (start/stop succeeded)
#   1   Generic error
#   2   Already running / not running mismatch
#   78  EX_CONFIG — refused to start (e.g. missing LOOM_USE_SPAWN_LOOP)

set -euo pipefail

# ─── Path resolution ────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Resolve repo root via git common-dir to handle invocation from worktrees.
if REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --git-common-dir 2>/dev/null)"; then
    if [[ ! "$REPO_ROOT" = /* ]]; then
        REPO_ROOT="$(cd "$SCRIPT_DIR" && cd "$REPO_ROOT" && pwd)"
    fi
    REPO_ROOT="$(dirname "$REPO_ROOT")"
else
    REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
fi

LOOM_DIR="$REPO_ROOT/.loom"
PIDFILE="$LOOM_DIR/spawn-loop.pid"
STATEFILE="$LOOM_DIR/spawn-loop-state.json"
STOP_SIGNAL="$LOOM_DIR/stop-spawn-loop"
LOGFILE="$LOOM_DIR/logs/spawn-loop.log"
LOCKS_DIR="$LOOM_DIR/locks"
CHECKPOINT_DIR="$LOOM_DIR/sweep-checkpoint"
DAEMON_PIDFILE="$LOOM_DIR/daemon-loop.pid"
SPAWN_CLAUDE="$REPO_ROOT/.loom/scripts/spawn-claude.sh"

# Defaults (overridable via env)
MAX_PARALLEL="${MAX_PARALLEL:-3}"
POLL_INTERVAL="${POLL_INTERVAL:-30}"
SHUTDOWN_GRACE_SEC="${SHUTDOWN_GRACE_SEC:-300}"

# ─── Logging ────────────────────────────────────────────────────────────────
log() {
    local level="$1"; shift
    local line
    line="$(date -u '+%Y-%m-%dT%H:%M:%SZ') [$level] $*"
    echo "$line" >&2
    mkdir -p "$(dirname "$LOGFILE")"
    echo "$line" >> "$LOGFILE" 2>/dev/null || true
}
log_info()  { log "INFO"  "$@"; }
log_warn()  { log "WARN"  "$@"; }
log_error() { log "ERROR" "$@"; }

iso_now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }

# ─── State file (JSON) ──────────────────────────────────────────────────────
# We use python3 for safe JSON read/write — bash JSON manipulation is fragile
# and the repo already requires python3 for token selection. State writes are
# atomic via .tmp + mv.
require_python() {
    if ! command -v python3 >/dev/null 2>&1; then
        log_error "python3 is required for state file management."
        exit 1
    fi
}

state_init() {
    mkdir -p "$LOOM_DIR" "$LOCKS_DIR" "$(dirname "$LOGFILE")"
    if [[ ! -f "$STATEFILE" ]]; then
        python3 - "$STATEFILE" <<'PY'
import json, sys, datetime, os
path = sys.argv[1]
data = {
    "started_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "running": []
}
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
os.replace(tmp, path)
PY
    fi
}

# Print pids of currently-tracked children (one per line).
state_list_running_pids() {
    [[ -f "$STATEFILE" ]] || return 0
    python3 - "$STATEFILE" <<'PY' 2>/dev/null || true
import json, sys
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
    for c in data.get("running", []):
        print(c.get("pid", ""))
except Exception:
    pass
PY
}

# Print issue numbers of currently-tracked children (one per line).
state_list_running_issues() {
    [[ -f "$STATEFILE" ]] || return 0
    python3 - "$STATEFILE" <<'PY' 2>/dev/null || true
import json, sys
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
    for c in data.get("running", []):
        print(c.get("issue", ""))
except Exception:
    pass
PY
}

state_count_running() {
    state_list_running_pids | grep -c -v '^$' || true
}

# Add a child to state.running.
state_add_child() {
    local issue="$1" pid="$2" token="${3:-unknown}"
    python3 - "$STATEFILE" "$issue" "$pid" "$token" <<'PY'
import json, sys, datetime, os
path, issue, pid, token = sys.argv[1:5]
with open(path) as f:
    data = json.load(f)
data.setdefault("running", []).append({
    "issue": int(issue),
    "pid": int(pid),
    "started_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "token": token,
})
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
os.replace(tmp, path)
PY
}

# Remove all children whose pid is no longer alive. Prints issue numbers of
# the removed entries (one per line) so the caller can decide whether to
# unlock / re-queue.
state_reap_dead() {
    [[ -f "$STATEFILE" ]] || return 0
    python3 - "$STATEFILE" <<'PY'
import json, os, sys, errno
path = sys.argv[1]
with open(path) as f:
    data = json.load(f)
alive, dead = [], []
for c in data.get("running", []):
    pid = int(c.get("pid", 0))
    try:
        os.kill(pid, 0)  # signal 0: existence check
        alive.append(c)
    except ProcessLookupError:
        dead.append(c)
    except PermissionError:
        # Process exists but we don't own it — count as alive (defensive).
        alive.append(c)
    except OSError as e:
        if e.errno == errno.ESRCH:
            dead.append(c)
        else:
            alive.append(c)
data["running"] = alive
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
os.replace(tmp, path)
for c in dead:
    print(c.get("issue", ""))
PY
}

# ─── Lock primitive (mkdir-based, POSIX-atomic) ─────────────────────────────
# Mirrors `loom_tools.claim.claim_issue` (#3236): mkdir is the only POSIX-
# atomic primitive available on stock macOS (flock is Linux-only). The
# directory's presence is the lock; we drop a tiny metadata file inside for
# debugging but the existence of the directory is the contract.
lock_path() { echo "$LOCKS_DIR/issue-$1"; }

acquire_lock() {
    local issue="$1"
    local lock
    lock="$(lock_path "$issue")"
    mkdir -p "$LOCKS_DIR"
    if mkdir "$lock" 2>/dev/null; then
        cat > "$lock/owner.json" <<EOF
{
  "issue": $issue,
  "owner_pid": $$,
  "acquired_at": "$(iso_now)"
}
EOF
        return 0
    fi
    return 1
}

release_lock() {
    local issue="$1"
    local lock
    lock="$(lock_path "$issue")"
    [[ -d "$lock" ]] || return 0
    rm -rf "$lock" 2>/dev/null || true
}

# ─── GitHub helpers ─────────────────────────────────────────────────────────
gh_args() {
    # Inject --repo if LOOM_REPO is set; otherwise gh uses the cwd's remote.
    if [[ -n "${LOOM_REPO:-}" ]]; then
        echo "--repo $LOOM_REPO"
    fi
}

list_ready_issues() {
    # shellcheck disable=SC2046
    gh issue list $(gh_args) \
        --label "loom:issue" \
        --state open \
        --limit 50 \
        --json number \
        --jq '.[].number' 2>/dev/null || true
}

issue_has_label() {
    local issue="$1" label="$2"
    # shellcheck disable=SC2046
    gh issue view "$issue" $(gh_args) --json labels --jq ".labels[].name" 2>/dev/null \
        | grep -Fxq "$label"
}

flip_label_to_building() {
    local issue="$1"
    # shellcheck disable=SC2046
    gh issue edit "$issue" $(gh_args) \
        --remove-label "loom:issue" \
        --add-label "loom:building" >/dev/null 2>&1
}

restore_label_to_ready() {
    local issue="$1"
    # shellcheck disable=SC2046
    gh issue edit "$issue" $(gh_args) \
        --remove-label "loom:building" \
        --add-label "loom:issue" >/dev/null 2>&1 || true
}

checkpoint_exists() {
    local issue="$1"
    [[ -f "$CHECKPOINT_DIR/issue-${issue}.json" ]]
}

# ─── Spawn ──────────────────────────────────────────────────────────────────
spawn_sweep() {
    local issue="$1"
    local log_path="$LOOM_DIR/logs/sweep-issue-${issue}.log"
    mkdir -p "$(dirname "$log_path")"

    if [[ ! -x "$SPAWN_CLAUDE" ]]; then
        log_error "spawn-claude.sh not executable at $SPAWN_CLAUDE"
        return 1
    fi

    # Append a header to the per-issue log so reruns are distinguishable.
    {
        echo ""
        echo "==== spawn-loop tick: $(iso_now) issue=$issue ===="
    } >> "$log_path" 2>/dev/null || true

    # Detach with setsid-equivalent (bash &) so the child survives the loop's
    # next iteration and we can simply track its PID. spawn-claude.sh handles
    # token selection; we record whatever it picked via the LOOM_TOKEN_NAME env
    # hint if available, otherwise "unknown".
    LOOM_TERMINAL_ID="spawn-$$-${issue}" \
        nohup "$SPAWN_CLAUDE" -p "/loom:sweep ${issue}" \
        >> "$log_path" 2>&1 &
    local pid=$!

    # Token is selected inside spawn-claude.sh; we don't know which one without
    # parsing the child's stderr. Record "unknown" — token attribution lives in
    # the per-issue log file and the bad-tokens manifest, not in spawn-loop
    # state.
    state_add_child "$issue" "$pid" "unknown"
    log_info "spawned issue=$issue pid=$pid log=$log_path"
}

# ─── Main loop ──────────────────────────────────────────────────────────────
tick() {
    # 1. Reap dead children. If an issue's child died but the issue is still
    #    `loom:building` and a checkpoint exists, the next ready-issue scan
    #    won't pick it back up (because the label is wrong). We re-arm by
    #    flipping the label back to `loom:issue` ourselves so the next tick
    #    can re-claim and re-spawn — sweep.md handles the actual phase skip
    #    via its checkpoint read on entry.
    local dead_issue
    while IFS= read -r dead_issue; do
        [[ -z "$dead_issue" ]] && continue
        release_lock "$dead_issue"
        if checkpoint_exists "$dead_issue" && issue_has_label "$dead_issue" "loom:building"; then
            log_warn "child for issue=$dead_issue died with checkpoint present; re-queueing"
            restore_label_to_ready "$dead_issue"
        else
            log_info "child for issue=$dead_issue exited cleanly (no checkpoint or already merged)"
        fi
    done < <(state_reap_dead)

    # 2. If we're at capacity or shutting down, skip the spawn phase.
    if [[ -f "$STOP_SIGNAL" ]]; then
        return 0
    fi
    local running
    running="$(state_count_running)"
    if (( running >= MAX_PARALLEL )); then
        return 0
    fi

    # 3. Fetch ready issues and try to claim until we hit MAX_PARALLEL.
    local ready_issues
    ready_issues="$(list_ready_issues || true)"
    if [[ -z "$ready_issues" ]]; then
        return 0
    fi

    # Track issues already claimed by our own children so we don't double-spawn
    # in the gap between flip_label_to_building and the next GitHub list query.
    local already
    already="$(state_list_running_issues | tr '\n' ' ' || true)"

    local issue
    while IFS= read -r issue; do
        [[ -z "$issue" ]] && continue
        running="$(state_count_running)"
        if (( running >= MAX_PARALLEL )); then
            break
        fi

        # Skip if we're already running this one.
        if [[ " $already " == *" $issue "* ]]; then
            continue
        fi

        # Atomic claim: lock-dir first (loses fast if another spawn-loop or
        # daemon got there), then label flip (loses if a parallel agent flipped
        # already). We release the lock if the label flip fails.
        if ! acquire_lock "$issue"; then
            log_info "issue=$issue lock held by another agent; skipping"
            continue
        fi
        if ! flip_label_to_building "$issue"; then
            log_warn "issue=$issue label flip failed; releasing lock"
            release_lock "$issue"
            continue
        fi

        spawn_sweep "$issue" || {
            log_error "spawn failed for issue=$issue; rolling back label + lock"
            restore_label_to_ready "$issue"
            release_lock "$issue"
            continue
        }
        already="$already $issue"
    done <<< "$ready_issues"
}

run_loop() {
    log_info "spawn-loop started (pid=$$ max_parallel=$MAX_PARALLEL poll=${POLL_INTERVAL}s)"
    # Coexistence warning (does not block).
    if [[ -f "$DAEMON_PIDFILE" ]]; then
        local dpid
        dpid="$(cat "$DAEMON_PIDFILE" 2>/dev/null || true)"
        if [[ -n "$dpid" ]] && kill -0 "$dpid" 2>/dev/null; then
            log_warn "loom-daemon is also running (pid=$dpid). Both will compete for loom:issue."
            log_warn "Recommended: stop the daemon (./.loom/scripts/daemon.sh stop) before relying on spawn-loop."
        fi
    fi

    # Trap signals so a SIGTERM from `kill` does graceful shutdown.
    trap 'log_info "received SIGTERM"; touch "$STOP_SIGNAL"' TERM
    trap 'log_info "received SIGINT";  touch "$STOP_SIGNAL"' INT

    while true; do
        # Tick body runs even during shutdown so dead children are reaped.
        tick || log_error "tick error: $?"

        if [[ -f "$STOP_SIGNAL" ]]; then
            log_info "stop signal detected; waiting up to ${SHUTDOWN_GRACE_SEC}s for children"
            local waited=0
            while (( waited < SHUTDOWN_GRACE_SEC )); do
                state_reap_dead >/dev/null
                local n
                n="$(state_count_running)"
                if (( n == 0 )); then
                    break
                fi
                sleep 5
                waited=$((waited + 5))
            done
            local n
            n="$(state_count_running)"
            if (( n > 0 )); then
                log_warn "shutdown timeout reached; $n children still running (leaving them)"
            else
                log_info "all children exited cleanly"
            fi
            rm -f "$STOP_SIGNAL" "$PIDFILE"
            log_info "spawn-loop exited"
            return 0
        fi

        sleep "$POLL_INTERVAL"
    done
}

# ─── Commands ───────────────────────────────────────────────────────────────
cmd_start() {
    if [[ "${LOOM_USE_SPAWN_LOOP:-}" != "1" ]]; then
        cat >&2 <<EOF
spawn-loop is opt-in. Set LOOM_USE_SPAWN_LOOP=1 to start:

  LOOM_USE_SPAWN_LOOP=1 $0 start

This guard exists so existing daemon users (./.loom/scripts/daemon.sh) are
not surprised by a competing orchestrator. See issue #3374.
EOF
        exit 78
    fi

    require_python

    if [[ -f "$PIDFILE" ]]; then
        local existing
        existing="$(cat "$PIDFILE" 2>/dev/null || true)"
        if [[ -n "$existing" ]] && kill -0 "$existing" 2>/dev/null; then
            echo "spawn-loop already running (pid=$existing)"
            exit 2
        fi
        rm -f "$PIDFILE"
    fi

    state_init
    rm -f "$STOP_SIGNAL"

    echo $$ > "$PIDFILE"
    run_loop
}

cmd_stop() {
    if [[ ! -f "$PIDFILE" ]]; then
        echo "spawn-loop not running"
        return 0
    fi
    local pid
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
        echo "spawn-loop PID file stale; removing"
        rm -f "$PIDFILE"
        return 0
    fi
    echo "Requesting graceful shutdown of spawn-loop (pid=$pid)..."
    touch "$STOP_SIGNAL"
    # Wait briefly so callers can `&& echo "stopped"` ergonomically.
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if (( waited >= 10 )); then
            echo "spawn-loop still draining children (this can take up to ${SHUTDOWN_GRACE_SEC}s)"
            return 0
        fi
    done
    echo "spawn-loop exited"
}

cmd_status() {
    if [[ ! -f "$PIDFILE" ]]; then
        echo "spawn-loop: not running"
        return 1
    fi
    local pid
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        local count
        count="$(state_count_running 2>/dev/null || echo 0)"
        echo "spawn-loop: running (pid=$pid, children=$count)"
        if [[ -f "$STATEFILE" ]]; then
            echo "state: $STATEFILE"
            python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print('  started_at:', d.get('started_at','?')); [print(f\"  - issue {c['issue']} pid {c['pid']} since {c['started_at']}\") for c in d.get('running', [])]" "$STATEFILE" 2>/dev/null || true
        fi
        return 0
    fi
    echo "spawn-loop: not running (stale PID file at $PIDFILE)"
    return 1
}

usage() {
    # Extract the leading comment block (from line 2 up to but not including
    # the first non-comment line, i.e. `set -euo pipefail`). Avoids
    # `head -n -1` which is GNU-only and breaks on macOS BSD head.
    awk 'NR>=2 { if (/^[^#]/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
}

main() {
    local cmd="${1:-}"
    shift || true
    case "$cmd" in
        start)  cmd_start "$@" ;;
        stop)   cmd_stop "$@" ;;
        status) cmd_status "$@" ;;
        ""|--help|-h) usage; exit 0 ;;
        *) echo "ERROR: unknown command '$cmd'" >&2; usage; exit 1 ;;
    esac
}

main "$@"
