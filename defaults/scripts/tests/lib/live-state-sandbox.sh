#!/usr/bin/env bash
# live-state-sandbox.sh — shared test sandbox for the loom-daemon's live-state
# file surface (issue #5179).
#
# Why this exists: this is the THIRD time a daemon lifecycle test has leaked
# into a real, live daemon's on-disk state because one more state file went
# un-isolated:
#
#   1. #4087 — a daemon test booted out the operator's real, running daemon.
#   2. #5131 — a test removed the live `autonomy-desired` marker while a real
#      daemon was running (fixed by exporting a scoped LOOM_AUTONOMY_MARKER).
#   3. #5179 — a test rewrote the LIVE host's `.daemon.pid` while a real
#      daemon was running (no `LOOM_PID_FILE` override existed anywhere),
#      producing a false "degraded" liveness verdict on a healthy host.
#
# Each prior fix enumerated ONE more surface as a per-variable override at the
# test's sandbox-setup call site. That pattern guarantees a fourth, fifth,
# ... Nth instance: every NEW daemon state file (a future heartbeat variant, a
# new registry, whatever comes next) is un-isolated by default until it is
# individually discovered leaking in production. This helper inverts that:
# ONE call isolates the WHOLE known live-state surface at once, so a state
# file this daemon does not have yet only needs one new line added HERE, not
# a new ad hoc override hunted down and added to the test file itself.
#
# Covers (env var -> what it isolates, and which script/module resolves it):
#   LOOM_PID_FILE        -> `.daemon.pid`         (loom-daemon's daemon_pidfile.rs,
#                            tier 1 -- an explicit override the daemon process
#                            itself always honors, though NOTE: the daemon
#                            lifecycle SHELL scripts recompute + re-export this
#                            unconditionally from $DAEMON_STATE_HOME rather than
#                            honoring an inbound value, so this export is a
#                            backstop for anything that resolves the pid file
#                            without going through those scripts -- the
#                            existence check below is what actually catches a
#                            shell-script-level leak)
#   LOOM_AUTONOMY_MARKER -> `autonomy-desired`    (autonomy_marker.rs / the
#                            lifecycle scripts' INTENT_MARKER, both honor an
#                            inbound override)
#   LOOM_SOCKET_PATH     -> the daemon socket, AND (as its directory) the
#                            default resolution root for `daemon.heartbeat`
#                            and the marker's own un-overridden default
#                            (daemon_heartbeat.rs / autonomy_marker.rs /
#                            loom-daemon-start.sh's `LOOM_DIR`)
#   LOOM_WORKSPACES_PATH -> `workspaces.json`, the multi-workspace registry
#                            (loom-daemon/src/workspace_registry.rs)
#
# Deliberately NOT covered here (already isolated by dedicated, existing
# helpers/exports a sourcing test is expected to keep using alongside this
# one): the launchd label (`lib/launchd-sandbox.sh`'s
# `launchd_sandbox_new_label`) and the systemd unit name (the sourcing test's
# own scratch `LOOM_SYSTEMD_UNIT` export) -- both are already
# per-suite-scoped identifiers, not files under a `.loom` directory, so they
# do not fit this helper's "one directory, one snapshot" model. This helper's
# job is the remaining live-STATE-FILE surface.
#
# Usage (source it):
#   source "$SCRIPT_DIR/lib/live-state-sandbox.sh"
#   live_state_sandbox_init "$BASE_WORKDIR"   # exports the 4 vars above
#   ...
#   before="$(live_state_sandbox_snapshot "$HOME" "$LOOM_REPO_ROOT")"
#   ... run the whole suite ...
#   after="$(live_state_sandbox_snapshot "$HOME" "$LOOM_REPO_ROOT")"
#   live_state_sandbox_report_changes "$before" "$after"   # empty == untouched

# live_state_sandbox_init <workdir>
#
# Exports LOOM_PID_FILE, LOOM_AUTONOMY_MARKER, LOOM_SOCKET_PATH, and
# LOOM_WORKSPACES_PATH so every one of them resolves under <workdir> --
# regardless of which per-scenario $HOME/$PWD a later sub-invocation uses.
# <workdir> is created if it does not already exist.
live_state_sandbox_init() {
    local workdir="$1"
    mkdir -p "$workdir"
    export LOOM_PID_FILE="$workdir/.daemon.pid"
    export LOOM_AUTONOMY_MARKER="$workdir/autonomy-desired"
    export LOOM_SOCKET_PATH="$workdir/daemon.sock"
    export LOOM_WORKSPACES_PATH="$workdir/workspaces.json"
}

# The basenames of every well-known daemon live-state file under a
# `<root>/.loom` directory. Kept as ONE list (see the module doc above) so a
# newly added daemon state file is added in exactly one place.
_LIVE_STATE_SANDBOX_FILENAMES=".daemon.pid autonomy-desired daemon.heartbeat workspaces.json"

# live_state_sandbox_real_state_paths <root>
#
# Echoes the well-known live-state file paths under <root>/.loom, one per
# line. <root> is normally $HOME (the machine-level state a real, host-wide
# daemon uses) and/or the checkout this suite itself lives in (the exact
# directory a "repo mode" daemon uses -- and the directory whose `.daemon.pid`
# was corrupted in the #5179 incident).
live_state_sandbox_real_state_paths() {
    local root="$1" name
    for name in $_LIVE_STATE_SANDBOX_FILENAMES; do
        echo "$root/.loom/$name"
    done
}

# _live_state_sandbox_fingerprint <path>
#
# "<absent>" when the path does not exist, else a fingerprint of its content.
# For every file EXCEPT `daemon.heartbeat` this is a full content checksum
# (byte-identical required), mirroring the #4381 production-binary checksum
# guard idiom already used elsewhere in this suite
# (test-loom-daemon-update.sh's `_prod_daemon_checksum`) -- these files are
# write-once-per-lifecycle-event (daemon start/relaunch, workspace
# registration), so a healthy, untouched daemon never rewrites them during a
# multi-minute test run.
#
# `daemon.heartbeat` is deliberately fingerprinted differently: a real, HEALTHY
# daemon on the same host rewrites its own heartbeat's timestamp on a fixed
# ~60s cadence as part of completely normal operation (daemon_heartbeat.rs),
# so a byte-identical requirement would false-fail on every run long enough to
# span a real heartbeat tick -- exactly the "verify while a real daemon is
# running" scenario this guard exists to support. What actually matters is
# WHICH process owns the file: extract just the `pid=<n>` token, so the
# fingerprint changes only when a DIFFERENT process starts heartbeating into
# this path (a real leak) rather than on every routine timestamp refresh from
# the same legitimate daemon.
_live_state_sandbox_fingerprint() {
    local path="$1"
    if [[ ! -e "$path" ]]; then
        echo "<absent>"
        return 0
    fi
    if [[ "$(basename "$path")" == "daemon.heartbeat" ]]; then
        grep -o 'pid=[0-9]*' "$path" 2>/dev/null | head -1
        return 0
    fi
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" 2>/dev/null | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" 2>/dev/null | awk '{print $1}'
    else
        echo "<no-checksum-tool>"
    fi
}

# live_state_sandbox_snapshot <root> [<root> ...]
#
# Prints one "<path>=<fingerprint>" line per well-known live-state path under
# every given root. Call once before the suite runs anything and again after
# it finishes; diff the two (or use live_state_sandbox_report_changes) to
# detect a write to a real `.loom` state path.
live_state_sandbox_snapshot() {
    local root path
    for root in "$@"; do
        while IFS= read -r path; do
            echo "$path=$(_live_state_sandbox_fingerprint "$path")"
        done < <(live_state_sandbox_real_state_paths "$root")
    done
}

# live_state_sandbox_report_changes <before-snapshot> <after-snapshot>
#
# Prints a unified diff of the two snapshots (empty output == nothing
# changed). Callers should treat non-empty output as a hard failure -- it
# means a REAL `.loom` live-state path was created or modified during the run.
live_state_sandbox_report_changes() {
    local before="$1" after="$2"
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after")
}
