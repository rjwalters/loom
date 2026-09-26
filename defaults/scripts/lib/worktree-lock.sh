#!/usr/bin/env bash
# lib/worktree-lock.sh
#
# The repo-global `git worktree add` concurrency lock behind worktree.sh's
# ALWAYS-TAKEN create path (#3380), its ownership-verified release
# (#6014/#6017), and — since #8195 slice 7 — a best-effort delegation to
# `loom-daemon worktree-lock acquire`/`release`
# (`loom-daemon/src/worktree_cli/lock.rs`, ported in slice 1 / #8226) ahead of
# the shell implementation kept below it as the fallback.
#
# WHY THE LOCK EXISTS AT ALL: `git worktree add` is not safe to run
# concurrently against the same repo — parallel invocations contend on the
# per-worktree administrative dir (`.git/worktrees/issue-N/`) and on git's
# repo-global locks. The observed failure mode in busy shepherd sessions was
# multi-minute hangs (10-20 min) while a peer process held an `index.lock` it
# would never release.
#
# WHY IT IS REPO-GLOBAL, NOT PER-ISSUE: `git worktree add` mutates the
# repo-global `.git/config.lock` (writing the new branch's upstream
# configuration), so two adds for DIFFERENT issues still race on it. The
# issue number is recorded as owner metadata only, for debugging visibility —
# see `owner.json` below.
#
# WHY `mkdir` IS THE PRIMITIVE: it is atomic on every filesystem Loom runs on.
# `flock` is not available on stock macOS, so `mkdir` is the only portable
# atomic filesystem operation available.
#
# OWNERSHIP VERIFICATION (#6014/#6017): each acquisition writes a random
# one-shot `token` into `owner.json` alongside `owner_pid`, and
# `acquire_worktree_lock` returns it via the `WORKTREE_LOCK_TOKEN` global.
# `release_worktree_lock` requires the caller to pass that same token back and
# refuses to remove the lock directory unless the token it finds on disk still
# matches — so a late release from a stale/wedged holder (e.g. its EXIT trap
# finally firing well after an operator judged it dead, manually cleared the
# lock, and a different process legitimately re-acquired it) is a safe no-op
# instead of deleting a live holder's lock out from under it. Stale-PID
# recovery happens exactly once per acquisition, so two processes racing to
# break the same dead lock cannot livelock breaking each other's fresh one.
#
# WHY A FALLBACK HERE, NOT A HARD DELEGATION (unlike worktree-remove/
# worktree-wip/worktree-cleanup/worktree-reset): this lock sits on
# worktree.sh's ALWAYS-TAKEN create path. Slice 1 tried a hard, `exec`-style
# delegation there and was reverted (#8226) — thirty hermetic shell suites
# with no built Rust binary all broke, because a missing/refusing binary is
# indistinguishable from "no lock taken" under an exec contract. There is also
# no safe "refuse" answer for a lock the way there is for a destructive verb:
# unlike `worktree-cleanup`'s no-op-when-absent degradation, skipping
# serialization outright on any host with no daemon would reopen the exact
# #3380 race (concurrent `git worktree add` invocations hanging on
# `.git/config.lock`) for the hosts most likely to lack a build — CI. So
# `acquire_worktree_lock`/`release_worktree_lock` below try the daemon FIRST
# and fall straight through to the historical mkdir-based implementation,
# UNCHANGED, on anything other than a clean 0 (acquired) or 1 (refused,
# timeout) answer from it. A missing binary, an installed daemon too old to
# know the `worktree-lock` subcommand family at all, or a malformed reply all
# read exactly like "no daemon" and get exactly today's shell behaviour.
# Nothing here can make a no-daemon host worse; a host with a current one
# gets the canonical, already-tested ownership-verified implementation
# instead of a second copy of it.
#
# Sourced (not exec'd) from worktree.sh, which is over the file-size ratchet
# threshold and therefore frozen (.loom/docs/file-size-policy.md) — moved out
# whole, with the delegation wrapper added here rather than inline, per that
# policy's own preferred remedy ("new sibling module, small dispatch arm left
# behind"). `print_warning` gets the same standalone fallback
# `lib/worktree-forge-pr-check.sh` uses, so this file also sources on its own
# (the retained test suite does exactly that).
#
# Tunables (env vars, documented in worktree.sh's own --help):
#   LOOM_WORKTREE_LOCK_TIMEOUT       — seconds to wait (default 600 = 10min)
#   LOOM_WORKTREE_LOCK_POLL_INTERVAL — seconds between poll attempts (default 2)

if ! declare -F print_warning >/dev/null 2>&1; then
    print_warning() { echo "WARNING: $1" >&2; }
fi

LOOM_WORKTREE_LOCK_TIMEOUT="${LOOM_WORKTREE_LOCK_TIMEOUT:-600}"
LOOM_WORKTREE_LOCK_POLL_INTERVAL="${LOOM_WORKTREE_LOCK_POLL_INTERVAL:-2}"

# Resolve the locks directory to the canonical git common dir so worktrees
# and the main workspace all share the same lock namespace. Falls back to the
# current dir for the rare case where we're not yet inside a repo (tests).
_worktree_locks_dir() {
    local common
    common=$(git rev-parse --git-common-dir 2>/dev/null || true)
    if [[ -n "$common" ]]; then
        # git-common-dir may be returned as a relative path; resolve it.
        local abs_common
        abs_common=$(cd "$common" 2>/dev/null && pwd) || abs_common="$common"
        echo "$(dirname "$abs_common")/.loom/locks"
    else
        echo ".loom/locks"
    fi
}

_worktree_lock_path() {
    # The argument is the issue number — accepted for owner-metadata logging
    # only. The lock itself is repo-global; see the module doc above.
    echo "$(_worktree_locks_dir)/worktree-add"
}

# The `loom-daemon` binary implementing `worktree-lock acquire`/`release`, or
# empty when none is resolvable. Resolved lazily and once per process, by
# acquire_worktree_lock below (mirrors worktree.sh's own
# `cleanup_partial_worktree_state`) — a caller that never creates a worktree
# never pays a subprocess for it. Stays empty (never an error) when
# `lib/locate-daemon-bin.sh` was not sourced by the caller, or found nothing —
# see the module doc above for why that reads identically to "no daemon"
# rather than aborting. A plain global, not a getter function, so
# `scripts/check-daemon-subcommand-versions.sh` — which recognises
# `"$var" <subcommand>` by tracing `var=$(...loom_resolve_self_daemon_bin...)`
# assignments textually — can see this dependency; every other delegating
# call site in worktree.sh follows the same shape.
# requires-daemon: worktree-lock optional   #8195 slice 7 — falls back to _worktree_lock_acquire_shell/_worktree_lock_release_shell, unchanged, on anything but a real 0/1 answer
_WT_LOCK_BIN_RESOLVED=""
_WT_LOCK_DAEMON_BIN=""

# Returns 0 if lock acquired, non-zero otherwise. Sets WORKTREE_LOCK_HOLDER_PID
# on timeout failure so the caller can include it in error output. On success,
# sets WORKTREE_LOCK_TOKEN to the one-shot acquisition token the caller MUST
# pass back to release_worktree_lock (see "Ownership verification" above).
WORKTREE_LOCK_HOLDER_PID=""
WORKTREE_LOCK_TOKEN=""
# Set by acquire_worktree_lock; read only by release_worktree_lock, so a token
# minted by one side of the acquire/release pair is always released by that
# same side — see release_worktree_lock's own comment below.
_WT_LOCK_DELEGATED=false

_worktree_lock_acquire_shell() {
    local issue="$1"
    local lock
    lock="$(_worktree_lock_path "$issue")"
    local locks_dir
    locks_dir="$(_worktree_locks_dir)"

    mkdir -p "$locks_dir" 2>/dev/null || true

    local deadline=$(( $(date +%s) + LOOM_WORKTREE_LOCK_TIMEOUT ))
    local stale_retry_done=0

    while true; do
        if mkdir "$lock" 2>/dev/null; then
            # Lock acquired; record owner metadata for debugging plus a
            # one-shot token so release can verify it still owns this lock
            # (issue #6014 — see "Ownership verification" above).
            local token
            token="$$-$(date -u +%s%N 2>/dev/null || date -u +%s)-$RANDOM"
            cat > "$lock/owner.json" <<EOF
{
  "issue": $issue,
  "owner_pid": $$,
  "token": "$token",
  "script": "worktree.sh",
  "acquired_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF
            WORKTREE_LOCK_TOKEN="$token"
            return 0
        fi

        # Lock exists. Check whether the owner is still alive; if not, clear
        # it once and retry (stale-lock recovery).
        local owner_pid=""
        if [[ -f "$lock/owner.json" ]]; then
            owner_pid=$(awk -F'[ ,]+' '/owner_pid/ {gsub(/[^0-9]/,"",$3); print $3; exit}' "$lock/owner.json" 2>/dev/null)
        fi

        if [[ -n "$owner_pid" ]] && [[ "$stale_retry_done" -eq 0 ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
            if [[ "${JSON_OUTPUT:-}" != "true" ]]; then
                print_warning "Stale worktree lock from dead PID $owner_pid — cleaning up"
            fi
            rm -rf "$lock" 2>/dev/null || true
            stale_retry_done=1
            continue
        fi

        if [[ $(date +%s) -ge $deadline ]]; then
            WORKTREE_LOCK_HOLDER_PID="$owner_pid"
            return 1
        fi

        sleep "$LOOM_WORKTREE_LOCK_POLL_INTERVAL"
    done
}

# _worktree_lock_release_shell <issue> <token>
#
# Removes the repo-global worktree-add lock ONLY if <token> matches the
# token currently recorded in owner.json — i.e. only if the caller is the
# process that most recently acquired it (issue #6014). A caller with a
# stale/empty token (already released, or never actually held the lock)
# leaves the directory untouched: there is nothing it can safely prove it
# owns, so removing anything would risk deleting a different, live holder's
# lock (the exact race described in issue #6014).
_worktree_lock_release_shell() {
    local issue="$1"
    local token="$2"
    [[ -z "$issue" ]] && return 0
    # No token means we never held the lock (or already released it) — never
    # remove a lock directory we cannot prove is ours.
    [[ -z "$token" ]] && return 0

    local lock
    lock="$(_worktree_lock_path "$issue")"
    [[ -d "$lock" ]] || return 0

    local current_token=""
    if [[ -f "$lock/owner.json" ]]; then
        current_token=$(awk -F'"' '/"token"[[:space:]]*:/ {print $4; exit}' "$lock/owner.json" 2>/dev/null)
    fi

    if [[ "$current_token" != "$token" ]]; then
        # The lock directory belongs to a different acquisition (ours was
        # already cleared and reassigned) — do NOT touch it.
        return 0
    fi

    rm -rf "$lock" 2>/dev/null || true
}

# acquire_worktree_lock <issue>  (#8195 slice 7 — tries the daemon first)
#
# Exit 0 with `TOKEN=<token>` on stdout from `loom-daemon worktree-lock
# acquire` is a real acquisition; exit 1 (with `HOLDER_PID=<pid>` when
# readable, otherwise nothing) is a real, considered refusal — both are
# ANSWERS from the same lock the shell implementation above also manages, so
# both are trusted and returned directly, PROVIDED stderr is empty. Real
# `worktree-lock acquire` never writes to stderr on either of those two
# answers — only its rc-2 `Unusable` branch does — so any stderr output at
# all means this was not that: a locks-directory error, or (found for real
# while testing this slice, #8195) a differently-shaped `loom-daemon` on
# PATH that happens to reuse exit code 1 for "I do not understand this
# subcommand" — a hand-rolled test double standing in for a real daemon in
# an unrelated suite's fixture (`test-worktree-forge-pr-check.sh`'s Test 8),
# not a daemon this file shipped with. Neither is an answer this wrapper
# trusts, and both fall straight through to _worktree_lock_acquire_shell
# exactly as if no daemon had been asked at all — same as rc 2 itself (locks
# dir unusable, or clap refusing an unrecognized subcommand on an older
# daemon) and any other exit code.
acquire_worktree_lock() {
    local issue="$1"
    if [[ -z "$_WT_LOCK_BIN_RESOLVED" ]]; then
        _WT_LOCK_DAEMON_BIN="$(loom_resolve_self_daemon_bin 2>/dev/null || true)"
        _WT_LOCK_BIN_RESOLVED=1
    fi
    _WT_LOCK_DELEGATED=false

    if [[ -n "$_WT_LOCK_DAEMON_BIN" ]]; then
        local out rc err stderr_file
        stderr_file="$(mktemp 2>/dev/null || echo /tmp/loom-wt-lock-acquire-stderr.$$)"
        out="$("$_WT_LOCK_DAEMON_BIN" worktree-lock acquire --issue "$issue" --owner-pid "$$" \
            --timeout "$LOOM_WORKTREE_LOCK_TIMEOUT" \
            --poll "$LOOM_WORKTREE_LOCK_POLL_INTERVAL" 2>"$stderr_file")"
        rc=$?
        err="$(cat "$stderr_file" 2>/dev/null)"
        rm -f "$stderr_file"
        if [[ -z "$err" ]]; then
            if [[ $rc -eq 0 && "$out" == TOKEN=* ]]; then
                # shellcheck disable=SC2034  # read by worktree.sh's own call sites
                WORKTREE_LOCK_TOKEN="${out#TOKEN=}"
                _WT_LOCK_DELEGATED=true
                return 0
            elif [[ $rc -eq 1 ]]; then
                WORKTREE_LOCK_HOLDER_PID=""
                # shellcheck disable=SC2034  # read by worktree.sh's own call sites
                [[ "$out" == HOLDER_PID=* ]] && WORKTREE_LOCK_HOLDER_PID="${out#HOLDER_PID=}"
                _WT_LOCK_DELEGATED=true
                return 1
            fi
        fi
        # Not an answer. Fall through below.
    fi

    _worktree_lock_acquire_shell "$issue"
}

# release_worktree_lock <issue> <token>  (#8195 slice 7 — tries the daemon
# first, but ONLY the side that actually acquired this token)
#
# $_WT_LOCK_DELEGATED is set by acquire_worktree_lock immediately above and
# read only here, so a token minted by the shell path is always released by
# the shell path even if a daemon becomes resolvable in between — it never
# does within one process, but nothing here depends on that not happening.
# `loom-daemon worktree-lock release` already treats an empty or
# already-cleared token as a safe no-op (#6014), matching
# _worktree_lock_release_shell's own contract exactly.
release_worktree_lock() {
    local issue="$1"
    local token="$2"

    if [[ "$_WT_LOCK_DELEGATED" == "true" ]]; then
        if [[ -n "$_WT_LOCK_DAEMON_BIN" ]]; then
            "$_WT_LOCK_DAEMON_BIN" worktree-lock release --token "$token" >/dev/null 2>&1 || true
            return 0
        fi
    fi

    _worktree_lock_release_shell "$issue" "$token"
}
