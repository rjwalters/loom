#!/usr/bin/env bash
# session-lock-sandbox.sh — keep a suite's session-exec dispatch locks out of
# the real ~/.loom (#10364).
#
# `loom-daemon session-exec host` takes a per-container lock file under
# `$LOOM_SESSION_LOCK_DIR`, defaulting to `$HOME/.loom/session-locks`. A suite
# that runs the real binary must never write there: fixture account names
# would litter the operator's directory, and on a fleet host one could match a
# real account. Source this IN PLACE OF the suite's own cleanup trap, with the
# suite's temp root:
#
#   TMPROOT="$(mktemp -d)"
#   source "$SCRIPT_DIR/lib/session-lock-sandbox.sh" "$TMPROOT"
#
# It points LOOM_SESSION_LOCK_DIR inside the temp root and installs the EXIT
# trap, which fails the suite if the real lock directory changed during the
# run (its entries, or it or anything in it newer than the start; `ls -lA`,
# so a change to `~/.loom` itself is not counted), then removes the temp
# root. The suite's own exit status is otherwise kept.

_lss_root="$1"
_lss_real="${HOME}/.loom/session-locks"
export LOOM_SESSION_LOCK_DIR="${_lss_root}/session-locks"
_lss_before="$(ls -lA "$_lss_real" 2>&1)"
: >"${_lss_root}/session-locks.marker"

_lss_exit() {
    local rc=$?
    local newer
    newer="$(find "$_lss_real" -newer "${_lss_root}/session-locks.marker" 2>/dev/null)"
    if [[ "$(ls -lA "$_lss_real" 2>&1)" != "$_lss_before" || -n "$newer" ]]; then
        echo "FAIL: this suite wrote under the real ${_lss_real} (#10364)" >&2
        rc=1
    fi
    rm -rf "$_lss_root"
    exit "$rc"
}
trap _lss_exit EXIT
