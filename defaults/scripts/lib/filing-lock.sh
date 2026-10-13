#!/usr/bin/env bash
# filing-lock.sh — machine-wide **issue-filing lock** (#6714). Bash half of
# `loom-daemon/src/filing_lock.rs`; both implement the identical on-disk
# protocol, so a shell filer and the daemon serialize against each other.
#
# Source this file (do not exec). It defines:
#
#   loom_filing_lock_acquire [label]
#       Take the machine-wide issue-filing lock, blocking at most
#       LOOM_FILING_LOCK_WAIT_SECS.
#         return 0  — safe to file (lock held, re-entrant, or degraded open)
#         return 75 — DEFER: could not acquire. The caller MUST NOT file; it
#                     defers its burst to the next tick and logs that it did.
#       Sets LOOM_FILING_LOCK_PATH to the held lock dir (empty when none) and
#       exports LOOM_FILING_LOCK_HELD=1 so a nested acquire is a no-op.
#
#   loom_filing_lock_release
#       Release the lock taken by the last acquire (idempotent, trap-safe).
#
# ---------------------------------------------------------------------------
# Why this exists
# ---------------------------------------------------------------------------
# #3707 ("parallel issue-creating agents race on `gh issue create` and
# cross-contaminate bodies") shipped a DOCUMENTATION-ONLY mitigation: "do not
# run concurrent Architects — serialize issue creation". On 2026-08-08 two
# Architects filing 5-issue bursts into DIFFERENT repos overlapped anyway, and
# one repo's five issue bodies were overwritten with the other's. Titles were
# untouched, so nothing looked wrong for 13 days.
#
# The convention could not hold, for reasons that are not operator error:
#
#   * the daemon is the scheduler — no human is in the loop at dispatch time,
#     so "do not run concurrent Architects" is not a thing anyone can comply
#     with; concurrent cross-repo Architects are NORMAL operation;
#   * `role_collision.rs`'s InProgressGuard serializes `(root, role)` — it is
#     per-workspace BY CONSTRUCTION and cannot serialize an Architect in one
#     repo against an Architect in another;
#   * `issue_creation_mutex.rs` is an in-PROCESS tokio mutex, and the agents
#     that race are separate OS processes.
#
# This lock is the mechanism the convention could not be. It wraps the actual
# filing call site — `create-issue.sh`, the single-sourced entry point every
# issue-creating role (Architect, Auditor, Curator/Builder decomposition,
# Doctor, Hermit, Judge) already goes through.
#
# ---------------------------------------------------------------------------
# Scope: one host
# ---------------------------------------------------------------------------
# Every workspace and every agent process on this machine get hard mutual
# exclusion via POSIX-atomic `mkdir` (`flock` is deliberately avoided;
# unavailable on stock macOS), matching the token-pool lock, the per-issue
# claim lock and lib/build-slot.sh. Two processes can only overwrite each
# other's body text through shared memory or a shared filesystem, i.e. on one
# machine, so this is what closes the 2026-08-08 hazard. A soft cross-host tier
# once rode the Matrix peer-claim channel; it was removed with it (#11112).
#
# ---------------------------------------------------------------------------
# On-disk protocol (shared with filing_lock.rs)
# ---------------------------------------------------------------------------
#   <store>/                  # ~/.loom/locks/issue-filing by default
#     holder/                 # the lock itself — mkdir-atomic
#       owner.json            # {"host","pid","label","acquired_at"}
#
# ---------------------------------------------------------------------------
# Four safety properties (all load-bearing)
# ---------------------------------------------------------------------------
#   1. A crashed holder cannot wedge fleet-wide issue creation. Two independent
#      reap legs: the holder dir aging past LOOM_FILING_LOCK_STALE_SECS, and —
#      for an owner recorded on THIS host — a dead owner PID, reaped at once.
#   2. Bounded and FAIL-SAFE. On timeout the caller DEFERS (75) rather than
#      filing unserialized. This is the one place this lock deliberately does
#      not copy lib/build-slot.sh, which degrades open: an unserialized build
#      wastes CPU, an unserialized filing burst corrupts issue bodies.
#   3. Degrades OPEN only when there is no lock to take (unusable store). A
#      store nobody can write is a store nobody is serialized by, so refusing
#      would convert a corruption risk into a filing outage.
#   4. Re-entrant. LOOM_FILING_LOCK_HELD short-circuits a nested acquire, so a
#      role that wraps a whole burst does not deadlock against the per-call
#      acquire inside create-issue.sh.
#
# Constants (env-tunable; keep in lock-step with filing_lock.rs):
#   LOOM_FILING_LOCK                default 1    0/false/off/no disables
#   LOOM_FILING_LOCK_WAIT_SECS      default 120  bounded wait before deferring
#   LOOM_FILING_LOCK_STALE_SECS     default 300  age at which a holder is reaped
#   LOOM_FILING_LOCK_DIR            default ~/.loom/locks/issue-filing
#   LOOM_FILING_LOCK_HELD           re-entrancy sentinel (set by the holder)

# Resolved lock dir currently held by this shell (empty = none).
LOOM_FILING_LOCK_PATH="${LOOM_FILING_LOCK_PATH:-}"

# The exit/return code meaning "deferred — do not file". 75 is EX_TEMPFAIL:
# "the caller is invited to retry", which is exactly the contract.
LOOM_FILING_LOCK_DEFER_RC=75

# Echo the machine-wide store directory.
loom_filing_lock_store() {
    local dir="${LOOM_FILING_LOCK_DIR:-}"
    dir="${dir#"${dir%%[![:space:]]*}"}"
    dir="${dir%"${dir##*[![:space:]]}"}"
    if [[ -n "$dir" ]]; then
        printf '%s\n' "$dir"
        return 0
    fi
    printf '%s\n' "${HOME:-/tmp}/.loom/locks/issue-filing"
}

# Echo a positive integer env value, or the given default when unset/invalid.
_loom_filing_lock_int() {
    local raw="${1:-}" default="$2"
    if [[ "$raw" =~ ^[1-9][0-9]*$ ]]; then
        printf '%s\n' "$raw"
    else
        printf '%s\n' "$default"
    fi
}

# True unless LOOM_FILING_LOCK is explicitly falsey.
loom_filing_lock_enabled() {
    case "$(printf '%s' "${LOOM_FILING_LOCK:-1}" | tr '[:upper:]' '[:lower:]')" in
        0 | false | off | no) return 1 ;;
        *) return 0 ;;
    esac
}

# True when this shell (or an ancestor) already holds the lock.
loom_filing_lock_held() {
    case "$(printf '%s' "${LOOM_FILING_LOCK_HELD:-}" | tr '[:upper:]' '[:lower:]')" in
        1 | true | yes | on) return 0 ;;
        *) return 1 ;;
    esac
}

# Host identity, mirroring sweep_registry::host_identity()'s precedence (the
# same helper sweep-lease-fence.sh uses) so the owner record this script writes
# is comparable with one written by the daemon.
loom_filing_lock_host() {
    if [[ -n "${LOOM_HOST_ID:-}" ]]; then printf '%s' "$LOOM_HOST_ID"; return 0; fi
    if [[ -n "${HOSTNAME:-}" ]]; then printf '%s' "$HOSTNAME"; return 0; fi
    local h
    h="$(hostname 2>/dev/null || true)"
    if [[ -n "$h" ]]; then printf '%s' "$h"; return 0; fi
    printf 'unknown-host'
}

# Age of a path in whole seconds. An unreadable mtime echoes -1 ("cannot age"),
# so a lock we cannot age is NEVER reaped by the mtime leg — the conservative
# direction, matching MkdirLock::is_stale in the Rust half.
_loom_filing_lock_age_secs() {
    local path="$1" mtime now
    # GNU `stat -c` first (an illegal option on BSD/macOS, so it fails cleanly
    # there), then BSD `stat -f %m`. The reverse order MISFIRES on GNU, where
    # `stat -f` means --file-system.
    mtime="$(stat -c %Y "$path" 2>/dev/null || true)"
    if [[ ! "$mtime" =~ ^[0-9]+$ ]]; then
        mtime="$(stat -f %m "$path" 2>/dev/null || true)"
    fi
    [[ "$mtime" =~ ^[0-9]+$ ]] || { echo -1; return 0; }
    now="$(date +%s)"
    if (( now > mtime )); then echo $(( now - mtime )); else echo 0; fi
}

# True when the current holder should be reaped as abandoned.
#   leg 1: owner recorded on THIS host with a PID that is not running —
#          decisive and immediate (a PID from another machine says nothing
#          about a process here, so it is never used).
#   leg 2: holder dir mtime past the stale threshold — the catch-all for a
#          holder we cannot probe.
_loom_filing_lock_abandoned() {
    local store="$1" self_host="$2" stale="$3"
    local holder="$store/holder" owner="$store/holder/owner.json"
    local o_host o_pid age
    if [[ -r "$owner" ]]; then
        o_host="$(sed -n 's/.*"host"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$owner" 2>/dev/null || true)"
        o_pid="$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p' "$owner" 2>/dev/null || true)"
        if [[ "$o_host" == "$self_host" && "$o_pid" =~ ^[1-9][0-9]*$ ]] \
            && ! kill -0 "$o_pid" 2>/dev/null; then
            return 0
        fi
    fi
    age="$(_loom_filing_lock_age_secs "$holder")"
    (( age >= 0 && age >= stale ))
}

# Describe the current blocker for the log line.
_loom_filing_lock_blocker() {
    local owner="$1/holder/owner.json" o_host o_pid o_label
    if [[ -r "$owner" ]]; then
        o_host="$(sed -n 's/.*"host"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$owner" 2>/dev/null || true)"
        o_pid="$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p' "$owner" 2>/dev/null || true)"
        o_label="$(sed -n 's/.*"label"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$owner" 2>/dev/null || true)"
        if [[ -n "$o_host" ]]; then
            printf "host %s pid %s is filing ('%s')" "$o_host" "${o_pid:-?}" "${o_label:-?}"
            return 0
        fi
    fi
    printf 'another filer holds the issue-filing lock'
}

# Acquire the machine-wide filing lock.
#   0  — safe to file
#   75 — DEFER (caller must not file)
loom_filing_lock_acquire() {
    local label="${1:-issue-filing}"
    LOOM_FILING_LOCK_PATH=""

    if loom_filing_lock_held; then
        echo "[filing-lock] already inside a filing lock (LOOM_FILING_LOCK_HELD) — re-entrant no-op for '${label}'" >&2
        return 0
    fi
    if ! loom_filing_lock_enabled; then
        echo "[filing-lock] LOOM_FILING_LOCK is off — issue-filing serialization disabled for '${label}'" >&2
        return 0
    fi

    local store wait_secs stale self_host
    store="$(loom_filing_lock_store)"
    wait_secs="$(_loom_filing_lock_int "${LOOM_FILING_LOCK_WAIT_SECS:-}" 120)"
    stale="$(_loom_filing_lock_int "${LOOM_FILING_LOCK_STALE_SECS:-}" 300)"
    self_host="$(loom_filing_lock_host)"

    if ! mkdir -p "$store" 2>/dev/null; then
        # Property 3: degrade OPEN. A store nobody can write is a store nobody
        # is serialized by; refusing would turn a corruption risk into an outage.
        echo "[filing-lock] WARNING: store ${store} is unusable — filing '${label}' WITHOUT serialization (degrading open)" >&2
        return 0
    fi

    local holder="$store/holder"
    local start now blocker logged_wait=0
    start="$(date +%s)"
    while :; do
        if mkdir "$holder" 2>/dev/null; then
            printf '{"host":"%s","pid":%s,"label":"%s","acquired_at":%s}\n' \
                "$self_host" "$$" "$label" "$(date +%s)" > "$holder/owner.json" 2>/dev/null || true
            LOOM_FILING_LOCK_PATH="$holder"
            export LOOM_FILING_LOCK_HELD=1
            now="$(date +%s)"
            echo "[filing-lock] acquired for '${label}' after $(( now - start ))s wait (${holder})" >&2
            return 0
        fi
        if [[ ! -d "$holder" ]]; then
            # mkdir failed and the path is not a directory ⇒ the store is
            # unusable (a FILE in the way, no permission). Degrade open.
            echo "[filing-lock] WARNING: lock path ${holder} is unusable — filing '${label}' WITHOUT serialization (degrading open)" >&2
            return 0
        fi
        if _loom_filing_lock_abandoned "$store" "$self_host" "$stale"; then
            echo "[filing-lock] reaping abandoned hold at ${holder} ($(_loom_filing_lock_blocker "$store"))" >&2
            rm -f "$holder/owner.json" 2>/dev/null || true
            rmdir "$holder" 2>/dev/null || true
            continue
        fi
        blocker="$(_loom_filing_lock_blocker "$store")"

        if (( logged_wait == 0 )); then
            echo "[filing-lock] '${label}' waiting up to ${wait_secs}s — ${blocker}" >&2
            logged_wait=1
        fi
        now="$(date +%s)"
        if (( now - start >= wait_secs )); then
            # Property 2: fail SAFE. Never file unserialized.
            echo "[filing-lock] DEFERRING '${label}': could not acquire the issue-filing lock within ${wait_secs}s (${blocker}). No issue was filed — retry on the next tick." >&2
            return "$LOOM_FILING_LOCK_DEFER_RC"
        fi
        sleep 1
    done
}

# Release the lock held by this shell. Idempotent; safe to call from a trap.
loom_filing_lock_release() {
    if [[ -n "${LOOM_FILING_LOCK_PATH:-}" ]]; then
        rm -f "$LOOM_FILING_LOCK_PATH/owner.json" 2>/dev/null || true
        rmdir "$LOOM_FILING_LOCK_PATH" 2>/dev/null || true
        echo "[filing-lock] released ${LOOM_FILING_LOCK_PATH}" >&2
        LOOM_FILING_LOCK_PATH=""
        unset LOOM_FILING_LOCK_HELD
    fi
    return 0
}
