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

binary="$1"
shift

stamp_file="${binary}.codesign-stamp"
lock_dir="${binary}.codesign-lock"

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

current_fingerprint="$(stat_fingerprint "$binary")"

needs_sign=1
if [ -n "$current_fingerprint" ] && [ -f "$stamp_file" ] \
    && [ "$(cat "$stamp_file" 2>/dev/null)" = "$current_fingerprint" ]; then
    needs_sign=0
fi

if [ "$needs_sign" = 1 ]; then
    # Acquire a short-lived spinlock so concurrent first-launches of a
    # freshly built binary don't all codesign it at once. Bounded so a
    # crashed holder (e.g. a runner killed mid-sign) can't wedge every future
    # invocation forever -- after ~5s we assume the lock is stale, clear it,
    # and proceed as if we acquired it.
    lock_attempts=0
    while ! mkdir "$lock_dir" 2>/dev/null; do
        lock_attempts=$((lock_attempts + 1))
        if [ "$lock_attempts" -ge 100 ]; then
            rmdir "$lock_dir" 2>/dev/null
            break
        fi
        sleep 0.05
    done

    # Re-check under the lock: another process may have signed and written
    # the stamp file while we were spinning for the lock.
    if [ -n "$current_fingerprint" ] && [ -f "$stamp_file" ] \
        && [ "$(cat "$stamp_file" 2>/dev/null)" = "$current_fingerprint" ]; then
        needs_sign=0
    fi

    if [ "$needs_sign" = 1 ]; then
        # Ad-hoc sign the binary to prevent _dyld_start verification hangs.
        # -f: force replace any existing signature
        # -s -: use ad-hoc identity (no developer certificate needed)
        codesign -f -s - "$binary" 2>/dev/null

        # codesign can itself touch the file's mtime; refresh the fingerprint
        # from the post-sign state so the stamp reflects what's actually on
        # disk (and future invocations compare against the right value).
        current_fingerprint="$(stat_fingerprint "$binary")"
        if [ -n "$current_fingerprint" ]; then
            printf '%s' "$current_fingerprint" > "$stamp_file" 2>/dev/null
        fi
    fi

    rmdir "$lock_dir" 2>/dev/null
fi

exec "$binary" "$@"
