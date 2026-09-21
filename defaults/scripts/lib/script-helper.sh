#!/usr/bin/env bash
# script-helper.sh — resolve + exec a native `loom-daemon` script-helper
# subcommand (issue #4275, epic #4081 Phase 3 family 5).
#
# Source this file (do not exec). Defines:
#
#   loom_exec_script_helper <subcommand> [args...]
#       execs `loom-daemon <subcommand> "$@"`, resolving the binary through
#       lib/locate-daemon-bin.sh. Never returns on success.
#
# WHICH BINARY A STUB EXECS (#8134)
#
# A stub needs the binary that IMPLEMENTS its subcommand, which is not always
# the binary `$LOOM_DAEMON_BIN` names. That variable means "the daemon this
# caller manages or probes" — the install whose version is compared, the
# endpoint a watchdog round-trips, a deliberately fake binary in a test — and
# a script that BOTH is a stub AND invokes a daemon has two different binaries
# in play at once. `loom-daemon-watchdog.sh` is the first of those: its
# retained suite pins `$LOOM_DAEMON_BIN` to a hanging mock to exercise the IPC
# probe, and before this split the stub exec'd that mock as the watchdog and
# never returned.
#
# So resolution here is:
#
#   1. $LOOM_DAEMON_SELF_BIN (loom_daemon_self_bin_override) — the explicit
#      "this is my implementation" seam, for a test harness or an operator.
#   2. Otherwise loom_locate_daemon_bin, UNCHANGED — $LOOM_DAEMON_BIN, then
#      $PATH, then the machine-level install, then a repo-local build.
#
# Tier 2 is deliberately the whole existing chain and not the rest of
# loom_resolve_self_daemon_bin's: in production the installed daemon IS the
# implementation, `$LOOM_DAEMON_BIN` must keep pinning it (that is what
# `loom update` and an operator debugging a stub both rely on), and hoisting a
# checkout-local build above it would be a silent behaviour change of exactly
# the kind #8134 rejected.
#
# This is the native replacement for `lib/loom-tools.sh`'s `run_loom_tool` on
# the six script-helper entry points (`strip-ansi.sh`, `resolve-model.sh`,
# `check-usage.sh`, `checkpoint.sh`, `sweep-experiment.sh`,
# `validate-phase.sh`). It exists for the same reason `run_loom_tool` did:
# resolution belongs in ONE place, so the stubs stay one line each and a
# consumer workspace with no Python (and no pip) still works.
#
# Exit-code discipline (load-bearing): `exec` replaces this shell, so the
# subcommand's own exit code reaches the caller unmodified. Several of these
# helpers use non-zero codes as *data* rather than errors — `resolve-model
# --tier` and `--task-alias` exit 3 to mean "no mapping, fall through to your
# normal precedence chain", and `loom-claim` uses 1/2/3/4 to distinguish
# already-claimed / bad args / not-found / wrong-agent. Any wrapper that
# swallowed or remapped those codes would silently change dispatch behavior,
# which is why nothing here inspects the child's status.
#
# When no `loom-daemon` can be resolved, prints an actionable error naming the
# provisioning path and exits 1 (there is no Python fallback any more — the
# Python modules these subcommands replaced were deleted in #4275).

# Find the repository root from a starting directory.
_lsh_find_repo_root() {
    local dir="${1:-$(pwd)}"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.git" ]] || [[ -d "$dir/.loom" ]]; then
            echo "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    return 1
}

# ---------------------------------------------------------------------------
# MARKER-DRIVEN DAEMON-VERSION PREFLIGHT (#8385, follow-up to #8285)
# ---------------------------------------------------------------------------
#
# A stub whose only statement is `exec "$bin" <sub>` has no version guard at
# all: against a binary that predates its subcommand, clap's own
#
#     error: unrecognized subcommand 'skip-labels'
#
# is the entire signal — it names neither the version to roll to nor the
# command to roll with. That is exactly what happened to `skip-labels.sh` on
# 2026-09-19 against this repo's own installed 0.19.179. #8285 gave merge-pr.sh
# an actionable refusal for the same class; the three functions below are the
# shared version for every stub driven by `loom_exec_script_helper`.
#
# OPT-IN BY DECLARATION, not by flag. The preflight fires only when the CALLING
# stub carries a `# requires-daemon: <this subcommand> >= <version>` marker —
# the same marker `scripts/check-daemon-subcommand-versions.sh` already
# enforces and `merge-pr.sh` already reads back at refusal time. A stub that
# declares nothing, declares `optional` (it probes and degrades), or declares a
# floor for a DIFFERENT subcommand is byte-identically unaffected, so this can
# be adopted one file at a time across the eleven-stub family. The marker is
# read out of the caller's own source, so the floor in the message, the floor
# the CI gate checks, and the floor a reviewer sees are one string.
#
# FAILS OPEN, deliberately — the opposite of merge-pr.sh's guards, for a reason
# worth stating. Those guards fail CLOSED because an empty answer there is
# indistinguishable from a real one and would silently close an unfinished
# issue. This is not a safety gate: its only job is to turn an unactionable
# error into an actionable one. So when the floor cannot be read, `--version`
# cannot be parsed, or the subcommand name is not marker-shaped, it does
# NOTHING and the exec proceeds — reproducing today's behaviour exactly, with
# the subcommand itself still refusing correctly when it is genuinely absent.
# Refusing on an unreadable version would invent a brand-new failure mode on a
# host whose binary is probably fine, which is a strictly worse trade.
#
# LOOM_SKIP_DAEMON_VERSION_PREFLIGHT=1 disables it wholesale, for a harness
# that deliberately pins an old binary.

# _lsh_declared_floor <caller-file> <subcommand> -- echo the declared minimum
# version for <subcommand>, or nothing.
#
# The grammar is check-daemon-subcommand-versions.sh's, narrowed to the one
# subcommand asked about: `# requires-daemon: <sub> >= <major.minor.patch>`,
# optionally indented, optionally followed by a free-text note. An `optional`
# declaration deliberately does not match — it is a statement that the stub
# copes without the subcommand, so there is no floor to enforce.
#
# `sed` with an early `q` rather than a bash read loop: one cheap subprocess
# that stops at the first marker, instead of iterating 3300 lines of
# worktree.sh in-shell on every WIP verb. <subcommand> is validated against
# ^[a-z][a-z0-9-]*$ by the caller BEFORE it reaches this interpolation, so no
# regex metacharacter can enter the script.
_lsh_declared_floor() {
    local caller="$1" sub="$2"
    [[ -n "$caller" && -r "$caller" ]] || return 0
    sed -n "/^[[:space:]]*#[[:space:]]*requires-daemon:[[:space:]]*${sub}[[:space:]][[:space:]]*>=/{
                s/^[[:space:]]*#[[:space:]]*requires-daemon:[[:space:]]*${sub}[[:space:]][[:space:]]*>=[[:space:]]*\([0-9][0-9.]*\).*$/\1/p
                q
            }" "$caller" 2>/dev/null || true
}

# _lsh_binary_version <bin> -- echo the semver `loom-daemon --version` reports,
# or nothing when it cannot be read. Never non-zero: every caller runs under
# `set -e`, and a best-effort diagnostic must not be able to abort the thing it
# is decorating (the same `|| true` discipline merge-pr.sh's hint documents).
_lsh_binary_version() {
    local bin="$1" out=""
    [[ -n "$bin" && -x "$bin" ]] || return 0
    out="$("$bin" --version 2>/dev/null || true)"
    out="${out%%$'\n'*}"
    if [[ "$out" =~ ([0-9]+\.[0-9]+\.[0-9]+) ]]; then
        printf '%s' "${BASH_REMATCH[1]}"
    fi
    return 0
}

# _lsh_version_lt <have> <want> -- true when have < want, compared NUMERICALLY
# per component. A string compare would read 0.19.9 as newer than 0.19.10 and
# 0.19.100 as older than 0.19.99, i.e. it would be wrong in both directions on
# exactly the two-digit-to-three-digit rollover this fleet lives on. Pure bash
# (no `sort -V` subprocess) and bash-3.2-clean, like every other shared helper
# here. A non-numeric component returns "not less" — fail open.
_lsh_version_lt() {
    local h="$1" w="$2" hp wp
    for _ in 1 2 3; do
        hp="${h%%.*}"
        wp="${w%%.*}"
        [[ "$hp" =~ ^[0-9]+$ && "$wp" =~ ^[0-9]+$ ]] || return 1
        (( 10#$hp < 10#$wp )) && return 0
        (( 10#$hp > 10#$wp )) && return 1
        case "$h" in *.*) h="${h#*.}" ;; *) h="0" ;; esac
        case "$w" in *.*) w="${w#*.}" ;; *) w="0" ;; esac
    done
    return 1
}

# _lsh_version_preflight <caller-file> <caller-dir> <subcommand> <bin>
#
# Returns 0 (proceed to the exec) in every case except one: the caller declares
# a floor, the binary reports a version, and that version is below the floor —
# then it prints the refusal and EXITS.
#
# The exit code is LOOM_SCRIPT_HELPER_MISSING_RC, the code each entry point
# already reserves for "could not run", never a fresh one. That is load-bearing
# and not stylistic: several of these subcommands use non-zero codes as DATA
# (`resolve-model --tier` exits 3 for "no mapping", `detect-dependency-cycle`
# exits 1 for "cycle found"), so a refusal wearing one of those codes would be
# read by the caller as an ANSWER — the precise reason that variable exists.
_lsh_version_preflight() {
    local caller="$1" caller_dir="$2" sub="$3" bin="$4"
    local floor="" have="" rc="${LOOM_SCRIPT_HELPER_MISSING_RC:-1}"

    [[ "${LOOM_SKIP_DAEMON_VERSION_PREFLIGHT:-0}" == "1" ]] && return 0
    [[ "$sub" =~ ^[a-z][a-z0-9-]*$ ]] || return 0

    floor="$(_lsh_declared_floor "$caller" "$sub")"
    [[ -n "$floor" ]] || return 0

    have="$(_lsh_binary_version "$bin")"
    [[ -n "$have" ]] || return 0

    _lsh_version_lt "$have" "$floor" || return 0

    {
        printf '[ERROR] the resolved loom-daemon is too old to run `loom-daemon %s`.\n\n' "$sub"
        printf '  Required:  >= %s\n' "$floor"
        printf '  Resolved:  %s (reports %s)\n' "$bin" "$have"
        printf '  Declared by %s:\n      # requires-daemon: %s >= %s\n\n' "$caller" "$sub" "$floor"
        printf 'Refused BEFORE the call, so you get the floor and the fix rather than the\n'
        printf 'binary'"'"'s own bare argument-parser error, which names neither (#8285/#8385).\n\n'
        printf 'Roll THIS host, artifact-first:\n\n'
        printf '    %s/cli/loom-daemon-update.sh --fetch\n\n' "$caller_dir"
        printf '…which resolves the newest published release >= the installed version, verifies\n'
        printf 'its checksum (and signature when present), provisions it, and restarts the daemon\n'
        printf 'under its supervisor.\n\n'
        printf 'If no release artifact carries %s yet — releases are cut at fleet-rollable\n' "$floor"
        printf 'boundaries, not on every VERSION bump (.loom/docs/release-cadence.md) — build it:\n\n'
        printf '    cargo build --release -p loom-daemon\n'
        printf '    export LOOM_DAEMON_BIN=<repo>/target/release/loom-daemon\n\n'
        printf '…or pin LOOM_DAEMON_BIN (or LOOM_DAEMON_SELF_BIN) to an existing build that\n'
        printf 'already has `%s`. Confirm before re-running:\n\n' "$sub"
        printf '    %s --version && %s %s --help\n\n' "$bin" "$bin" "$sub"
        printf 'Exiting %s — this entry point'"'"'s "could not run" code, never an answer.\n' "$rc"
    } >&2
    exit "$rc"
}

# loom_daemon_version_preflight <subcommand> <resolved-bin>
#
# The PUBLIC, standalone form of the preflight, for a stub that does its own
# resolution and its own `exec`. `skip-labels.sh` is the first: it is a Shape-A
# `stub` in scripts/shell-allowlist.txt, a category machine-checked on the
# requirement that its LAST code line IS the `exec`
# (scripts/check-shell-allowlist.sh), so it cannot hand the exec to
# `loom_exec_script_helper` without forfeiting that category. One call one line
# above its own exec gets it the identical guard.
#
# Same contract as the in-`loom_exec_script_helper` path in every respect:
# inert without a marker for <subcommand> in the CALLER's file, fails open on
# an unreadable version, exits LOOM_SCRIPT_HELPER_MISSING_RC on a genuine
# floor violation. ${BASH_SOURCE[1]} is the calling stub, exactly as it is
# inside loom_exec_script_helper — which is what lets the marker live in the
# stub that owns the dependency rather than in this library.
loom_daemon_version_preflight() {
    local caller="${BASH_SOURCE[1]:-$0}" caller_dir
    caller_dir="$(cd "$(dirname "$caller")" 2>/dev/null && pwd)" || caller_dir="."
    _lsh_version_preflight "$caller" "$caller_dir" "${1:-}" "${2:-}"
}

# LOOM_SCRIPT_HELPER_MISSING_RC — the exit code used when no loom-daemon can be
# resolved, and (since #8385) when a resolved binary is below a declared
# `# requires-daemon:` floor. Defaults to 1.
#
# A stub whose subcommand uses non-zero codes as DATA must override this, or a
# missing binary is indistinguishable from an answer. `detect-dependency-cycle`
# exits 1 to mean "cycle found" and `detect-startable-subset` exits 1 to mean
# "no subset declared"; a caller branching on the code alone would read a
# missing binary as a detected cycle. Those stubs set it to 2, which every one
# of these entry points already reserves for "could not run".
loom_exec_script_helper() {
    local subcommand="$1"
    shift

    # ${BASH_SOURCE[1]:-$0} hardens the caller-frame lookup the same way
    # run_loom_tool did (#3680): every call site is a bash-shebang'd script, so
    # BASH_SOURCE is populated, but the bare bashism would break if that changed.
    local script_dir repo_root bin caller_file
    caller_file="${BASH_SOURCE[1]:-$0}"
    script_dir="$(cd "$(dirname "$caller_file")" && pwd)"
    repo_root="$(_lsh_find_repo_root "$script_dir")" || repo_root=""

    # LOOM_DAEMON_SELF_BIN: which binary IMPLEMENTS this stub, as distinct from
    # LOOM_DAEMON_BIN, which means "the loom-daemon binary a script should
    # INVOKE". Those are the same thing for every port so far, and they are NOT
    # the same for a script that invokes loom-daemon itself: the watchdog's
    # retained suite sets LOOM_DAEMON_BIN to a MOCK so it can drive the IPC
    # probe, and a stub resolving through it would exec the mock as its own
    # implementation. Test 13's mock is `while true; do sleep 1; done`, so that
    # presents as a hang rather than a failure (#8134).
    #
    # Unset in production, where the two ARE the same and the normal resolution
    # below applies unchanged. Checked first, so a harness can pin the real
    # binary without disturbing what LOOM_DAEMON_BIN means to everything else.
    #
    # The #8385 preflight applies HERE too, not only on the resolution path
    # below. This seam is "pin the binary that implements me", not "skip my
    # checks" — a harness that pins a stale build should get the same
    # actionable refusal an operator would, and LOOM_SKIP_DAEMON_VERSION_
    # PREFLIGHT=1 is the explicit way to ask for the old behaviour.
    if [[ -n "${LOOM_DAEMON_SELF_BIN:-}" && -x "${LOOM_DAEMON_SELF_BIN}" ]]; then
        _lsh_version_preflight "$caller_file" "$script_dir" "$subcommand" "${LOOM_DAEMON_SELF_BIN}"
        exec "${LOOM_DAEMON_SELF_BIN}" "$subcommand" "$@"
    fi

    # shellcheck source=/dev/null
    source "$(dirname "${BASH_SOURCE[0]}")/locate-daemon-bin.sh"

    # $LOOM_DAEMON_SELF_BIN first (the implementation), then the normal
    # resolution completely unchanged — see "WHICH BINARY A STUB EXECS" above.
    # Both tiers are defined in the resolver library, not here: this file stays
    # glue, and every "which loom-daemon?" question is answered in one place.
    bin="$(loom_daemon_self_bin_override || loom_locate_daemon_bin "$repo_root")"

    if [[ -n "$bin" ]]; then
        _lsh_version_preflight "$caller_file" "$script_dir" "$subcommand" "$bin"
        exec "$bin" "$subcommand" "$@"
    fi

    printf '%s\n\n' "[ERROR] loom-daemon not found (needed for '$subcommand')." >&2
    echo "This script is a thin stub over the native \`loom-daemon $subcommand\`" >&2
    echo "subcommand (issue #4275). Provide a binary by either:" >&2
    echo "  - setting LOOM_DAEMON_SELF_BIN=/path/to/loom-daemon (the binary that IMPLEMENTS this subcommand, checked first) or LOOM_DAEMON_BIN=/path/to/loom-daemon, or" >&2
    if [[ -n "$repo_root" && -d "$repo_root/loom-daemon" ]]; then
        echo "  - building it: cargo build --release --manifest-path $repo_root/loom-daemon/Cargo.toml" >&2
    else
        echo "  - re-running the Loom installer, which provisions loom-daemon onto PATH" >&2
    fi
    exit "${LOOM_SCRIPT_HELPER_MISSING_RC:-1}"
}
