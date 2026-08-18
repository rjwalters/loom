#!/bin/bash
# macOS test runner for cargo
#
# Workaround for _dyld_start hangs on macOS ARM64 (issue #2298).
# Ad-hoc signs test binaries before execution to satisfy macOS
# code signature verification, preventing dyld from hanging
# during binary load.
#
# Configured via .cargo/config.toml:
#   [target.aarch64-apple-darwin]
#   runner = ".cargo/macos-test-runner.sh"
#
# --- Concurrent codesigning race (issue #6451) ------------------------------
#
# `cargo nextest run --workspace` runs every test in its own process, so a
# large test binary (e.g. loom-daemon's) has many concurrent processes
# exec'ing it during a single `nextest run`. Naively re-signing the binary
# in place (`codesign -f`, the original behavior) on *every* invocation
# rewrites the on-disk file while other, already-running processes still
# have it mapped as an executable. macOS's kernel-enforced code-signature
# validation on mapped executable pages responds to the backing file
# changing out from under an already-mapped process by SIGKILLing it. This
# showed up as a mass tail-of-run SIGKILL cascade once nextest's concurrency
# slots saturated onto one remaining large binary (many concurrent processes
# of the same binary, maximizing the collision window).
#
# Fix: sign at most once per "build generation" instead of once per process
# launch. A generation is identified by the binary's (mtime, size)
# fingerprint, which cargo changes on every rebuild and leaves untouched for
# the remainder of a `cargo nextest run` (the build phase completes before
# any test process execs, so the binary is never rewritten mid-run except by
# this script). We cache the last-signed fingerprint in a sidecar stamp file
# next to the binary and skip re-signing when the current fingerprint still
# matches it.
#
# This cannot regress the original #2298 _dyld_start fix: a binary that has
# genuinely changed since it was last signed (a real rebuild, or a binary
# nextest has never seen before) always has a different/absent fingerprint
# and is always signed before its first exec here, same as before.
#
# A short mkdir-based spinlock guards the check-then-sign region so the
# *first* burst of concurrent processes for a freshly built (or never-seen)
# binary don't all race to `codesign` at once before any of them has written
# the stamp file. The lock is released before `exec`, so it never blocks or
# delays an already-signed, steady-state run -- the common case takes the
# fast path below (no lock, no codesign) entirely.
#
# Two properties of that lock matter for correctness, both learned the hard
# way in review of this script:
#
#   1. The stamp comparison must re-stat the binary *inside* the lock, never
#      reuse a fingerprint read before waiting for the lock. `codesign`
#      touches the binary's mtime, so a waiter's pre-wait fingerprint can
#      never equal the stamp the lock winner just wrote -- the waiter would
#      conclude it must sign too and rewrite the file out from under the
#      winner, which by then has already released the lock and exec'd with
#      the binary mapped executable. That is exactly the SIGKILL race this
#      script exists to close.
#
#   2. Force-clearing a lock on a timeout is not the same as holding it. The
#      lock records its holder's PID; on timeout we clear it only if that PID
#      is provably gone (crashed mid-sign), and then re-race `mkdir` for real
#      ownership rather than assuming the clear granted it. A *live* holder
#      is simply waited on for further rounds, so genuine contention can
#      never degenerate into several processes calling `codesign` on the same
#      file concurrently.

binary="$1"
shift

stamp_file="${binary}.codesign-stamp"
lock_dir="${binary}.codesign-lock"
lock_pid_file="${lock_dir}/pid"

# Spinlock bounds: one round is LOCK_SPIN_ATTEMPTS * LOCK_SPIN_SLEEP (~5s),
# after which liveness of the holder is checked; LOCK_MAX_ROUNDS rounds bound
# the total wait (~60s) so nothing here can hang a test run forever.
LOCK_SPIN_ATTEMPTS=100
LOCK_SPIN_SLEEP=0.05
LOCK_MAX_ROUNDS=12

# Fingerprint a file as "<mtime> <size>". Tries BSD stat (macOS, the actual
# runtime target of this script) first, then GNU stat (Linux) so the
# skip-decision logic here can also be exercised/tested off macOS.
stat_fingerprint() {
    local f="$1"
    local out
    out="$(stat -f '%m %z' "$f" 2>/dev/null)" && [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    out="$(stat -c '%Y %s' "$f" 2>/dev/null)" && [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    return 1
}

# True when the binary's *current* on-disk fingerprint matches the stamp
# written by whoever last signed it -- i.e. no signing is needed.
#
# This always re-stats the binary. Every caller (the pre-lock fast path and
# the under-lock re-check) therefore compares against fresh state, which is
# what makes property (1) above hold.
stamp_matches() {
    local fp
    fp="$(stat_fingerprint "$binary")" || return 1
    [ -n "$fp" ] || return 1
    [ -f "$stamp_file" ] || return 1
    [ "$(cat "$stamp_file" 2>/dev/null)" = "$fp" ]
}

lock_holder_alive() {
    local pid
    pid="$(cat "$lock_pid_file" 2>/dev/null)"
    case "$pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    kill -0 "$pid" 2>/dev/null
}

release_lock() {
    rm -f "$lock_pid_file" 2>/dev/null
    rmdir "$lock_dir" 2>/dev/null
    return 0
}

# Acquire the lock, returning 0 only when this process genuinely owns it.
# A holder that is still alive is waited on; a holder that is gone has its
# lock cleared, after which we go back to racing `mkdir` like everyone else.
acquire_lock() {
    local round=0
    local attempts
    while [ "$round" -lt "$LOCK_MAX_ROUNDS" ]; do
        attempts=0
        while [ "$attempts" -lt "$LOCK_SPIN_ATTEMPTS" ]; do
            if mkdir "$lock_dir" 2>/dev/null; then
                printf '%s' "$$" > "$lock_pid_file" 2>/dev/null
                return 0
            fi
            attempts=$((attempts + 1))
            sleep "$LOCK_SPIN_SLEEP"
        done
        if ! lock_holder_alive; then
            # Crashed (or pre-PID-file legacy) holder: clear the stale lock,
            # then loop to contend for it properly. Clearing is NOT acquiring.
            release_lock
        fi
        round=$((round + 1))
    done
    return 1
}

sign_and_stamp() {
    # Ad-hoc sign the binary to prevent _dyld_start verification hangs.
    # -f: force replace any existing signature
    # -s -: use ad-hoc identity (no developer certificate needed)
    codesign -f -s - "$binary" 2>/dev/null

    # codesign can itself touch the file's mtime; refresh the fingerprint
    # from the post-sign state so the stamp reflects what's actually on
    # disk (and future invocations compare against the right value).
    local fp
    fp="$(stat_fingerprint "$binary")"
    if [ -n "$fp" ]; then
        printf '%s' "$fp" > "$stamp_file" 2>/dev/null
    fi
    return 0
}

if ! stamp_matches; then
    if acquire_lock; then
        # Re-check under the lock (fresh stat): another process may have
        # signed and written the stamp while we were spinning.
        if ! stamp_matches; then
            sign_and_stamp
        fi
        release_lock
    else
        # A live holder kept the lock for the whole bounded wait (~60s, far
        # longer than any real codesign call). Re-check first -- normally the
        # holder has long since stamped it. Only if the binary still looks
        # unsigned do we sign unlocked, because failing to sign a genuinely
        # changed binary would resurrect the #2298 _dyld_start hang, which is
        # the worse failure. This is a last resort, not the normal path.
        if ! stamp_matches; then
            sign_and_stamp
        fi
    fi
fi

exec "$binary" "$@"
