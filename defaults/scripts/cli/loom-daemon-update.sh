#!/usr/bin/env bash
# loom-daemon-update.sh - Self-update the RAW loom-daemon process (Issue #3968)
#
# Closes the "self-update gap" observed during the 2026-07-25/26 canary
# rollout: the daemon's self-repair loop filed AND fixed 16 of its own
# defects, but every merged fix only took effect after an operator manually
# rebuilt the Rust binary, reprovisioned it, and restarted the process. This
# script is the single operator command that does all three, in order,
# preserving the FLAGS-OFF/opt-in autonomy contract across the restart.
#
# Staleness detection strategy (primary, zero-network): compare the git
# commit BAKED INTO the currently-resolved `loom-daemon` binary (embedded at
# build time via build.rs -> LOOM_DAEMON_GIT_COMMIT, surfaced in
# `loom-daemon --version`) against the LOCAL source tree's current HEAD short
# commit. This answers the directly actionable question — "would rebuilding
# right now produce a different binary?" — without touching the network.
# Secondary (advisory only, never gates the rebuild): a bounded, best-effort
# check of how far local HEAD is behind origin/<default-branch>, mirroring
# check-main-freshness.sh's pattern, so an operator running this cron-style
# learns "you're up to date with local HEAD, but HEAD itself is behind
# origin" instead of being told nothing needs doing.
#
# It:
#   - detects whether the resolved binary is stale vs. the local source tree,
#   - rebuilds (`cargo build --release`) in loom-daemon/ when stale (or
#     --force),
#   - provisions the fresh binary to wherever the resolved binary lives
#     (LOOM_DAEMON_BIN override, else the machine-level ~/.local/bin install
#     via scripts/install/provision-daemon.sh, matching #3922's convention),
#   - reads the flags loom-daemon-start.sh persisted at the last invocation
#     (.loom/.daemon.flags, #3968) and restarts with EXACTLY those flags —
#     never more, never fewer. A daemon that was NOT running is left
#     stopped (this script never widens FLAGS-OFF by starting autonomy that
#     wasn't already running).
#
# Launchd-managed daemons (#4042): on Darwin the daemon is commonly launchd-
# managed (default since #3972/#4054), in which case NEITHER .loom/.daemon.pid
# nor .loom/.daemon.flags reliably reflects "is it running" — the pid file goes
# stale after any KeepAlive:SuccessfulExit relaunch, and a hand-bootstrapped
# daemon has no state files at all. This script therefore checks the launchd job
# state (`launchctl print gui/<uid>/<label>`, mirroring loom-daemon-stop.sh)
# AHEAD of the pid-file tier when resolving whether/how the daemon is running.
# When launchd-managed, it restarts via the `loom-daemon restart` primitive
# (#4077 — sends Request::RestartDaemon over the IPC socket; the supervised
# daemon exits 0 and launchd relaunches it onto the fresh binary with the
# plist's persisted ProgramArguments/EnvironmentVariables). .daemon.flags is NOT
# consulted in this mode (the plist's EnvironmentVariables IS the durable flag
# source), and no "restarting FLAGS-OFF" warning fires. If the running (old)
# binary predates #4077 and refuses the request, this script REFUSES LOUDLY
# (exit 6) and prints how to re-render the plist + relaunch under supervision
# (loom-daemon-update.sh --relaunch), rather than reporting a half-update — the
# exact #4011 silent-autonomy-loss class this closes. The old advice to bootstrap
# the EXISTING plist was itself a bug (#4118): it relaunched under the STALE plist
# (no KeepAlive:SuccessfulExit, no LOOM_DAEMON_SUPERVISOR), so every subsequent
# roll hit the same exit 6 forever, and its bootout killed in-flight sweeps
# (sweep children are direct children of the launchd job). --relaunch re-renders
# via loom-daemon-start.sh (installing the supervised keys) while preserving the
# live plist's LOOM_* autonomy env, and SIGTERMs the daemon so sweep children
# reparent instead of being torn down with the job.
#
# Usage:
#   ./.loom/scripts/cli/loom-daemon-update.sh              Detect, rebuild if stale, provision, restart (preserving flags)
#   ./.loom/scripts/cli/loom-daemon-update.sh --check       Detect only; exit 0 (up to date) or 3 (update available); no writes
#   ./.loom/scripts/cli/loom-daemon-update.sh --dry-run     Print the plan without building/provisioning/restarting
#   ./.loom/scripts/cli/loom-daemon-update.sh --force       Rebuild + provision + restart even if already up to date
#   ./.loom/scripts/cli/loom-daemon-update.sh --no-restart  Rebuild + provision only; leave the running daemon untouched
#   ./.loom/scripts/cli/loom-daemon-update.sh --relaunch    Launchd only: after a refused restart, re-render the plist and relaunch under supervision (SIGTERMs the daemon so sweep children reparent; preserves the live plist's LOOM_* env)
#   ./.loom/scripts/cli/loom-daemon-update.sh --help
#
# Environment:
#   LOOM_DAEMON_BIN       Path to the loom-daemon binary (else auto-detected,
#                          same resolution as loom-daemon-start.sh). When set,
#                          the fresh binary is provisioned directly to this
#                          exact path instead of the machine-level default.
#   LOOM_DAEMON_BIN_DIR   Machine-level install dir (default ~/.local/bin),
#                          forwarded to provision-daemon.sh.
#   LOOM_DAEMON_LAUNCHD    macOS only: 0/false/no disables ALL launchd interaction
#                          (ownership detection + launchd restart), symmetric with
#                          loom-daemon-start.sh / loom-daemon-stop.sh. A daemon
#                          started with --no-launchd / LOOM_DAEMON_LAUNCHD=0 gets
#                          an update that never reads the machine-global launchd
#                          domain and follows the PID-file/nohup restart path.
#   LOOM_LAUNCHD_LABEL     macOS only: the LaunchAgent label to inspect/restart
#                          (default com.rjwalters.loom-daemon).
#   LOOM_DAEMON_UPDATE_RELAUNCH  macOS/launchd only: 1/true/yes is equivalent to
#                          passing --relaunch (opt in to the re-render + relaunch
#                          on a refused restart).
#
# Exit codes:
#   0  up to date (no-op) OR rebuild+provision+restart succeeded
#   1  usage error / not a source checkout / build or provision failure
#   3  (--check only) update available
#   4  build verification FAILED: the freshly-built binary's embedded commit
#      does not match the source HEAD it was built from. This is a BUILD-SYSTEM
#      defect (a stale baked-in commit — e.g. a build.rs watch-set bug), NOT a
#      compile failure, and retrying cannot fix it; the script refuses to
#      provision the mis-stamped binary (#4053).
#   5  post-provision verification FAILED: the destination binary after a
#      claimed-successful provision is not the expected build (a silent no-op
#      roll — "reports success while shipping nothing"). Distinct from both a
#      compile failure and a provisioning soft-failure (#4053).
#   6  launchd restart FAILED: the daemon is launchd-managed but the running
#      (old) binary refused the `loom-daemon restart` IPC request (a pre-#4077
#      binary with no RestartDaemon handler, or a dead socket). The fresh binary
#      IS provisioned but the OLD one is still running; the script refuses to
#      report success. Without --relaunch it prints how to re-render the plist and
#      relaunch under supervision, then exits 6; with --relaunch (or
#      LOOM_DAEMON_UPDATE_RELAUNCH=1) it performs that re-render+relaunch itself,
#      propagating loom-daemon-start.sh's exit code (#4042, #4118).
#
# See also: loom-daemon-start.sh (writes .loom/.daemon.flags), loom-daemon-stop.sh
# (SIGTERM -> grace -> SIGKILL; in-flight sweeps survive by design — this
# script relies on that: stopping+restarting the dispatcher never kills
# dispatched work), scripts/install/provision-daemon.sh (machine-level
# provisioning, #3922).

set -uo pipefail

# ---------- output helpers ----------
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
    # before `set -uo pipefail`), stripping the leading "# ".
    awk 'NR>=2 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

# ---------- locate the daemon binary (mirrors loom-daemon-start.sh) ----------
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

# Extract the short commit from `loom-daemon --version` output, e.g.
# "loom-daemon 0.15.0 (commit ab12cd3, built 2026-07-26T12:00:00Z)" -> ab12cd3
extract_commit() {
    echo "$1" | grep -oE 'commit [0-9a-f]+' | head -n1 | awk '{print $2}'
}

# verify_destination_binary <dest_path> — assert the provisioned binary at
# <dest_path> embeds the expected source-HEAD commit (#4053). This is the
# direct answer to "reports success while shipping nothing": after a provision
# step returns success, the destination must actually be the freshly-built
# binary. Exits 5 on mismatch — distinguishable from a compile failure (exit 1)
# and from a provisioning soft-failure. Skipped only when the source HEAD is
# unknown (a tarball build with no .git), where there is nothing to compare
# against. Relies on $SOURCE_COMMIT being resolved (it is, before any build).
verify_destination_binary() {
    local dest="$1"
    if [[ "$SOURCE_COMMIT" == "unknown" ]]; then
        warn "Source HEAD is unknown (no .git?) — skipping post-provision verification."
        return 0
    fi
    if [[ -z "$dest" || ! -x "$dest" ]]; then
        err "Post-provision verification FAILED: provisioning reported success but no executable binary was found at the destination ('${dest:-<unknown>}')."
        exit 5
    fi
    local dest_version dest_commit
    dest_version=$("$dest" --version 2>/dev/null || true)
    dest_commit=$(extract_commit "$dest_version")
    if [[ "$dest_commit" != "$SOURCE_COMMIT" ]]; then
        err "Post-provision verification FAILED: destination binary at $dest embeds commit '${dest_commit:-<none>}' but the expected source HEAD is '$SOURCE_COMMIT'."
        err "Provisioning reported success yet the destination is NOT the freshly-built binary — a silent no-op roll. This is distinct from a compile failure and from a provisioning soft-failure; refusing to report success."
        exit 5
    fi
    ok "Post-provision verification: destination binary at $dest embeds source HEAD commit ($dest_commit)."
}

# ---------- args ----------
DRY_RUN=false
FORCE=false
CHECK_ONLY=false
NO_RESTART=false
RELAUNCH=false
[[ "${LOOM_DAEMON_UPDATE_RELAUNCH:-}" =~ ^(1|true|yes)$ ]] && RELAUNCH=true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h) show_help; exit 0 ;;
        --dry-run) DRY_RUN=true; shift ;;
        --force) FORCE=true; shift ;;
        --check) CHECK_ONLY=true; shift ;;
        --no-restart) NO_RESTART=true; shift ;;
        --relaunch) RELAUNCH=true; shift ;;
        *) err "Unknown option '$1'"; echo "Use --help for usage" >&2; exit 1 ;;
    esac
done

REPO_ROOT=$(find_repo_root)
if [[ -z "$REPO_ROOT" ]]; then
    err "Not in a Loom workspace (.loom directory not found)"
    exit 1
fi

DAEMON_DIR="$REPO_ROOT/loom-daemon"
if [[ ! -f "$DAEMON_DIR/Cargo.toml" ]]; then
    err "No loom-daemon/Cargo.toml found at $DAEMON_DIR."
    echo "loom-daemon-update.sh rebuilds FROM SOURCE and only works inside a Loom source checkout." >&2
    exit 1
fi

PID_FILE="$REPO_ROOT/.loom/.daemon.pid"
FLAGS_FILE="$REPO_ROOT/.loom/.daemon.flags"
START_SCRIPT="$REPO_ROOT/.loom/scripts/cli/loom-daemon-start.sh"
STOP_SCRIPT="$REPO_ROOT/.loom/scripts/cli/loom-daemon-stop.sh"

# ---------- staleness detection ----------
DAEMON_BIN=$(locate_daemon_bin "$REPO_ROOT")

INSTALLED_COMMIT="unknown"
if [[ -n "$DAEMON_BIN" && -x "$DAEMON_BIN" ]]; then
    installed_version_output=$("$DAEMON_BIN" --version 2>/dev/null || true)
    extracted=$(extract_commit "$installed_version_output")
    [[ -n "$extracted" ]] && INSTALLED_COMMIT="$extracted"
fi

SOURCE_COMMIT=$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo "unknown")

echo "Installed binary: ${DAEMON_BIN:-<none found>} (commit ${INSTALLED_COMMIT})"
echo "Source tree HEAD:  ${SOURCE_COMMIT}"

UPDATE_NEEDED=false
if [[ -z "$DAEMON_BIN" ]]; then
    echo "No loom-daemon binary currently resolvable (LOOM_DAEMON_BIN / PATH / loom-daemon/target/release) — a build is needed."
    UPDATE_NEEDED=true
elif [[ "$INSTALLED_COMMIT" == "unknown" || "$SOURCE_COMMIT" == "unknown" ]]; then
    warn "Could not determine one or both commits (installed=$INSTALLED_COMMIT, source=$SOURCE_COMMIT) — staleness unknown; treating as needing a rebuild to be safe."
    UPDATE_NEEDED=true
elif [[ "$INSTALLED_COMMIT" != "$SOURCE_COMMIT" ]]; then
    UPDATE_NEEDED=true
fi

# ---------- advisory: local HEAD vs origin/<default-branch> (non-blocking) ----------
# Never gates the rebuild decision above — purely informational, mirrors
# check-main-freshness.sh's bounded-fetch pattern.
advisory_behind_origin() {
    local repo_root="$1"
    # shellcheck disable=SC1091
    if [[ -r "$SCRIPT_DIR/../lib/default-branch.sh" ]]; then
        source "$SCRIPT_DIR/../lib/default-branch.sh" 2>/dev/null || return 0
    else
        return 0
    fi
    declare -F loom_default_branch >/dev/null 2>&1 || return 0
    local branch
    branch="$(cd "$repo_root" && loom_default_branch origin 2>/dev/null)" || return 0
    [[ -z "$branch" ]] && return 0
    if command -v timeout >/dev/null 2>&1; then
        (cd "$repo_root" && timeout 5 git fetch origin "$branch" --quiet >/dev/null 2>&1) || true
    else
        (cd "$repo_root" && git fetch origin "$branch" --quiet >/dev/null 2>&1) || true
    fi
    local n
    n="$(cd "$repo_root" && git rev-list --count "${branch}..origin/${branch}" 2>/dev/null || echo 0)"
    [[ "$n" =~ ^[0-9]+$ ]] || n=0
    if [[ "$n" -gt 0 ]]; then
        warn "note: local ${branch} is ${n} commit(s) behind origin/${branch} — rebuilding now will NOT pick those up. Run 'git merge --ff-only origin/${branch}' first if you want them."
    fi
}
advisory_behind_origin "$REPO_ROOT"

# ---------- launchd ownership detection (macOS, mirrors loom-daemon-stop.sh #4042) ----------
# launchd is checked AHEAD of the .loom/.daemon.pid tier because the plist's
# KeepAlive:SuccessfulExit assigns a FRESH pid on every supervised relaunch, so
# the pid file goes stale after the first relaunch even for a launchd job that
# loom-daemon-start.sh itself started; a hand-bootstrapped daemon has no state
# files at all. Honors LOOM_DAEMON_LAUNCHD symmetrically with start/stop.sh so a
# --no-launchd install never reaches into the machine-global launchd domain.
IS_DARWIN=false
[[ "$(uname -s)" == "Darwin" ]] && IS_DARWIN=true
USE_LAUNCHD="$IS_DARWIN"
if [[ "${LOOM_DAEMON_LAUNCHD:-}" =~ ^(0|false|no)$ ]]; then
    USE_LAUNCHD=false
fi
DEFAULT_LAUNCHD_LABEL="com.rjwalters.loom-daemon"
LAUNCHD_LABEL="${LOOM_LAUNCHD_LABEL:-$DEFAULT_LAUNCHD_LABEL}"
LAUNCHD_SERVICE="gui/$(id -u)/${LAUNCHD_LABEL}"
LAUNCHD_PLIST="$HOME/Library/LaunchAgents/${LAUNCHD_LABEL}.plist"

launchd_job_loaded() {
    [[ "$USE_LAUNCHD" == "true" ]] || return 1
    command -v launchctl >/dev/null 2>&1 || return 1
    launchctl print "$LAUNCHD_SERVICE" >/dev/null 2>&1
}
launchd_job_pid() {
    launchctl print "$LAUNCHD_SERVICE" 2>/dev/null | awk -F'= ' '/^[[:space:]]*pid = /{gsub(/[^0-9]/, "", $2); print $2; exit}'
}

# ---------- re-render + relaunch on a refused restart (#4118) ----------
# The exit-6 fallback USED to tell the operator to `launchctl bootstrap` the
# EXISTING plist. That plist is stale by construction (it is the pre-#4077 file
# that caused the refused restart) — bootstrapping it relaunches WITHOUT
# KeepAlive:SuccessfulExit and WITHOUT LOOM_DAEMON_SUPERVISOR, so the next roll
# refuses identically, forever; and its `launchctl bootout` tears down the whole
# job tree, killing in-flight sweeps (they are direct children of the launchd
# job). The correct fix is to RE-RENDER the plist via loom-daemon-start.sh (which
# hardcodes the two supervised keys), preserving the live plist's autonomy/auth
# env, and to stop the old daemon gracefully so sweep children reparent.

# harvest_plist_env <plist> — echo the live plist's EnvironmentVariables,
# restricted to exactly the keys render_launchd_plist itself forwards (LOOM_*,
# GH_TOKEN, GITEA_TOKEN, FORGE_TOKEN) and EXCLUDING:
#   - PATH / HOME     (start.sh rebuilds PATH from the live shell; round-tripping
#                      the plist's already-extended PATH would grow it each roll),
#   - LOOM_DAEMON_SUPERVISOR (start.sh hardcodes it; re-exporting is pointless).
# Emits one "<key>\t<base64(value)>" line per key so values containing spaces or
# newlines survive. Fails loudly (return 2) when the plist is absent or
# unparseable — it must NEVER silently return an empty set, which would let the
# re-render narrow the autonomy flags to FLAGS-OFF defaults (the #4011 class).
harvest_plist_env() {
    local plist="$1"
    if [[ ! -f "$plist" ]]; then
        err "Cannot harvest launchd env: plist not found at $plist"
        return 2
    fi
    if ! command -v plutil >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
        err "Cannot harvest launchd env: plutil and jq are both required on the macOS launchd path."
        return 2
    fi
    local json
    json=$(plutil -convert json -o - "$plist" 2>/dev/null) || {
        err "Cannot harvest launchd env: plist at $plist is not parseable by plutil."
        return 2
    }
    printf '%s' "$json" | jq -r '
        .EnvironmentVariables // {}
        | to_entries[]
        | select(.key | test("^(LOOM_[A-Za-z0-9_]*|GH_TOKEN|GITEA_TOKEN|FORGE_TOKEN)$"))
        | select(.key != "LOOM_DAEMON_SUPERVISOR")
        | .key + "\t" + (.value | @base64)
    ' 2>/dev/null || {
        err "Cannot harvest launchd env: failed to extract EnvironmentVariables from $plist."
        return 2
    }
}

# perform_relaunch <plist> <service> — re-render the LaunchAgent and relaunch it
# under launchd supervision, preserving the live plist's autonomy/auth env.
# Invoked ONLY from the exit-6 fallback when the operator opted in (--relaunch /
# LOOM_DAEMON_UPDATE_RELAUNCH=1), so the sweep-disrupting relaunch is a consented
# action, never silent. Returns loom-daemon-start.sh's exit code (or 6 if the env
# harvest fails — refusing to relaunch into a silently-narrowed env).
perform_relaunch() {
    local plist="$1"
    echo "--relaunch: re-rendering the LaunchAgent and relaunching under launchd supervision."

    # 1. Preserve the live plist's autonomy/auth env across the re-render.
    local harvested
    if ! harvested=$(harvest_plist_env "$plist"); then
        err "Refusing to relaunch: could not read the live plist's EnvironmentVariables."
        err "Relaunching now would silently narrow the autonomy flags to FLAGS-OFF defaults (#4011) — aborting."
        return 6
    fi
    local k v64 count=0
    while IFS=$'\t' read -r k v64; do
        [[ -z "$k" ]] && continue
        export "$k=$(printf '%s' "$v64" | base64 --decode)"
        count=$((count + 1))
    done <<< "$harvested"
    echo "Preserved ${count} LOOM_*/token env var(s) from the live plist across the re-render (PATH/HOME/LOOM_DAEMON_SUPERVISOR excluded by design)."

    # 2. Stop the old daemon GRACEFULLY so its sweep children reparent and keep
    #    working, instead of `launchctl bootout` tearing down the whole job tree.
    #    kill -TERM makes the daemon exit non-zero, so the stale plist's
    #    KeepAlive=false does not relaunch it — start.sh below installs the fresh,
    #    supervised plist and bootstraps the new process.
    local daemon_pid
    daemon_pid=$(launchd_job_pid)
    if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
        echo "Sending SIGTERM to the running daemon (pid ${daemon_pid}) — sweep children reparent and keep working (NOT bootout, which would kill them)."
        kill -TERM "$daemon_pid" 2>/dev/null || true
        local _waited
        for _waited in 1 2 3 4 5; do
            kill -0 "$daemon_pid" 2>/dev/null || break
            sleep 1
        done
    fi

    # 3. Re-render + bootstrap via loom-daemon-start.sh. It hardcodes
    #    KeepAlive:{SuccessfulExit:true} + LOOM_DAEMON_SUPERVISOR=launchd, and
    #    harvests the LOOM_* env we just re-exported. In launchd mode the plist's
    #    EnvironmentVariables — not .daemon.flags — is the durable config, so no
    #    flags are passed here.
    echo "Invoking ${START_SCRIPT} to re-render the supervised plist and relaunch."
    "$START_SCRIPT"
}

# Resolve which manager owns the running daemon: launchd (checked first), the
# .loom/.daemon.pid file (nohup/script-managed), or none. WAS_RUNNING is derived
# from this — a launchd-loaded job counts as running regardless of pid-file state.
DAEMON_MANAGER="none"
WAS_RUNNING=false
if launchd_job_loaded; then
    DAEMON_MANAGER="launchd"
    WAS_RUNNING=true
elif [[ -f "$PID_FILE" ]]; then
    existing_pid=$(cat "$PID_FILE" 2>/dev/null || true)
    if [[ -n "$existing_pid" ]] && kill -0 "$existing_pid" 2>/dev/null; then
        DAEMON_MANAGER="pidfile"
        WAS_RUNNING=true
    fi
fi

describe_manager() {
    case "$DAEMON_MANAGER" in
        launchd) echo "Running daemon manager: launchd (label ${LAUNCHD_LABEL})." ;;
        pidfile) echo "Running daemon manager: PID-file/nohup (.loom/.daemon.pid)." ;;
        *)       echo "Running daemon manager: not running." ;;
    esac
}

# ---------- --check: report only, no writes ----------
if [[ "$CHECK_ONLY" == "true" ]]; then
    describe_manager
    if [[ "$UPDATE_NEEDED" == "true" ]]; then
        warn "Update available (installed=${INSTALLED_COMMIT}, source=${SOURCE_COMMIT})."
        exit 3
    fi
    ok "loom-daemon binary is already up to date with source HEAD (${SOURCE_COMMIT})."
    exit 0
fi

if [[ "$FORCE" == "true" && "$UPDATE_NEEDED" == "false" ]]; then
    echo "--force given: rebuilding even though the binary already matches source HEAD."
    UPDATE_NEEDED=true
fi

if [[ "$UPDATE_NEEDED" == "false" ]]; then
    ok "loom-daemon binary is already up to date with source HEAD (${SOURCE_COMMIT}). Nothing to do."
    exit 0
fi

# ---------- resolve the restart plan up front (read-only; safe for --dry-run) ----------
# WAS_RUNNING + DAEMON_MANAGER were resolved above (launchd checked ahead of the
# pid file). The flags below are only consulted for the pid-file/nohup restart
# path — a launchd-managed restart replays flags from the plist, not this file.
RESTART_ARGS=()
FLAGS_SOURCE="none (defaulting to FLAGS-OFF bare restart)"
if [[ -f "$FLAGS_FILE" ]]; then
    FLAGS_SOURCE="$FLAGS_FILE"
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        RESTART_ARGS+=("$line")
    done < "$FLAGS_FILE"
fi

DEST_DIR="${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}"
PROVISION_TARGET="${LOOM_DAEMON_BIN:-$DEST_DIR/loom-daemon}"

if [[ "$DRY_RUN" == "true" ]]; then
    echo
    echo "[dry-run] Would run: (cd $DAEMON_DIR && cargo build --release)"
    echo "[dry-run] Would provision the fresh binary to: $PROVISION_TARGET"
    if [[ "$NO_RESTART" == "true" ]]; then
        echo "[dry-run] --no-restart given: would leave the running daemon (if any) untouched."
    elif [[ "$DAEMON_MANAGER" == "launchd" ]]; then
        echo "[dry-run] loom-daemon is launchd-managed (label ${LAUNCHD_LABEL}) — would restart via '$PROVISION_TARGET restart' (the #4077 supervised primitive); .daemon.flags is NOT consulted (the plist's EnvironmentVariables carries the equivalent config)."
    elif [[ "$WAS_RUNNING" == "true" ]]; then
        echo "[dry-run] Would stop + restart loom-daemon with flags from ${FLAGS_SOURCE}: ${RESTART_ARGS[*]:-<none>}"
    else
        echo "[dry-run] loom-daemon is not currently running — would NOT start it (this script never widens FLAGS-OFF by starting autonomy that wasn't already running)."
    fi
    exit 0
fi

# ---------- rebuild ----------
if ! command -v cargo >/dev/null 2>&1; then
    err "cargo not found on PATH — cannot rebuild loom-daemon."
    exit 1
fi

echo
echo "Rebuilding loom-daemon (cargo build --release)..."
if ! (cd "$DAEMON_DIR" && cargo build --release); then
    err "cargo build --release failed — the running daemon (if any) was left untouched."
    exit 1
fi

NEW_BIN=""
for candidate in \
    "$DAEMON_DIR/target/release/loom-daemon" \
    "$REPO_ROOT/target/release/loom-daemon"; do
    # `cargo build --release` run from loom-daemon/ writes to that crate's own
    # target/ when loom-daemon is a standalone crate, but to the WORKSPACE
    # root's target/ when it is a member of a Cargo workspace (this repo's
    # actual layout: root Cargo.toml -> [workspace] members = [...,
    # "loom-daemon"]). Check both, matching locate_daemon_bin()'s candidate
    # order above.
    if [[ -x "$candidate" ]]; then
        NEW_BIN="$candidate"
        break
    fi
done
if [[ -z "$NEW_BIN" ]]; then
    err "Build did not produce an executable at $DAEMON_DIR/target/release/loom-daemon or $REPO_ROOT/target/release/loom-daemon"
    exit 1
fi
ok "Build succeeded: $NEW_BIN"

# ---------- verify the freshly-built binary embeds the expected commit ----------
# A rebuild can succeed (exit 0) yet bake in a STALE LOOM_DAEMON_GIT_COMMIT — the
# exact hazard this script exists to close (a build.rs watch-set bug that lets
# `--version` report the old commit). Provisioning such a binary would "report
# success while shipping nothing" and, worse, turn any auto-update loop that
# trusts the baked commit into an infinite rebuild-still-stale retry. So assert
# the built commit == source HEAD BEFORE provisioning. On mismatch, fail loudly
# and do NOT provision: this is a build-system defect that retrying cannot fix,
# distinct from the compile failure handled above (#4053).
BUILT_VERSION_OUTPUT=$("$NEW_BIN" --version 2>/dev/null || true)
BUILT_COMMIT=$(extract_commit "$BUILT_VERSION_OUTPUT")
if [[ "$SOURCE_COMMIT" == "unknown" ]]; then
    warn "Source HEAD is unknown (no .git?) — skipping built-commit verification (tarball build)."
elif [[ -z "$BUILT_COMMIT" ]]; then
    err "Build verification FAILED: the freshly-built binary reports no commit in --version output ('${BUILT_VERSION_OUTPUT:-<empty>}')."
    err "Refusing to provision a binary that cannot prove what it was built from. This is a build-system defect, not a compile failure."
    exit 4
elif [[ "$BUILT_COMMIT" != "$SOURCE_COMMIT" ]]; then
    err "Build verification FAILED: the freshly-built binary embeds commit '$BUILT_COMMIT' but source HEAD is '$SOURCE_COMMIT'."
    err "A successful build produced a binary stamped with the WRONG commit (a stale baked-in commit — e.g. a build.rs watch-set bug). Retrying will not fix it; refusing to provision (#4053)."
    exit 4
else
    ok "Build verification: freshly-built binary embeds source HEAD commit ($BUILT_COMMIT)."
fi

# ---------- sign (Darwin-only, best-effort, non-fatal, #4016) ----------
# Ad-hoc-sign the freshly built binary with a stable identifier BEFORE
# provisioning, so both provisioning branches below (the LOOM_DAEMON_BIN
# override and provision_machine_daemon) copy an already-signed binary — the
# Mach-O signature survives `install`/`cp`. Signing does NOT make a TCC grant
# survive a rebuild (see sign_daemon_binary's own doc comment in
# scripts/install/provision-daemon.sh and .loom/docs/daemon-reference.md); it
# only pins a human-legible identifier in place of the rustc metadata hash.
# shellcheck disable=SC1091
if [[ -r "$REPO_ROOT/scripts/install/provision-daemon.sh" ]]; then
    source "$REPO_ROOT/scripts/install/provision-daemon.sh"
fi
if declare -F sign_daemon_binary >/dev/null 2>&1; then
    sign_daemon_binary "$NEW_BIN"
fi

# ---------- provision ----------
if [[ -n "${LOOM_DAEMON_BIN:-}" ]]; then
    # Explicit operator override — provision directly to that exact path
    # (the one loom-daemon-start.sh will resolve to next via LOOM_DAEMON_BIN),
    # rather than the machine-level default.
    dest="$LOOM_DAEMON_BIN"
    if install -m 755 "$NEW_BIN" "$dest" 2>/dev/null || { cp -f "$NEW_BIN" "$dest" 2>/dev/null && chmod 755 "$dest" 2>/dev/null; }; then
        ok "Provisioned loom-daemon -> $dest"
    else
        err "Failed to provision to LOOM_DAEMON_BIN=$dest"
        exit 1
    fi
    # This override path has the same "shipped nothing" hazard as the
    # machine-level path — verify the destination is the freshly-built binary.
    verify_destination_binary "$dest"
else
    if declare -F provision_machine_daemon >/dev/null 2>&1; then
        # Hard-fail on provisioning failure: a soft warn here (the pre-#4053
        # behavior) left the exit code at 0, which is exactly the "reports
        # success while shipping nothing" defect this issue closes.
        if ! provision_machine_daemon "$NEW_BIN"; then
            err "Machine-level provisioning FAILED (see above). Refusing to report success; the freshly-built binary is at $NEW_BIN — set LOOM_DAEMON_BIN=$NEW_BIN to use it directly."
            exit 1
        fi
        # provision_machine_daemon exports the destination it wrote to (even on
        # the version-equality short-circuit) — verify that destination is the
        # expected build so the short-circuit can no longer produce a silent
        # no-op on a real roll (#4053).
        verify_destination_binary "${PROVISIONED_DAEMON_BIN:-}"
    else
        warn "scripts/install/provision-daemon.sh not found/sourceable — skipping machine-level provisioning."
        warn "Freshly-built binary: $NEW_BIN (set LOOM_DAEMON_BIN=$NEW_BIN to use it directly)"
    fi
fi

# ---------- restart (preserve prior flags exactly — Issue #3968) ----------
if [[ "$NO_RESTART" == "true" ]]; then
    ok "Rebuilt + provisioned. Skipping restart (--no-restart)."
    if [[ "$WAS_RUNNING" == "true" ]]; then
        if [[ "$DAEMON_MANAGER" == "launchd" ]]; then
            echo "The running (launchd-managed) daemon is still the PRE-update binary. Restart it with:"
            echo "  $PROVISION_TARGET restart      (graceful: supervised in-place relaunch, in-flight sweeps preserved)"
            echo "If that binary predates #4077 and refuses the restart, re-render + relaunch under supervision:"
            echo "  loom-daemon-update.sh --relaunch   (preserves the live plist's LOOM_* env; SIGTERMs the daemon so sweep children reparent)"
            echo "Do NOT 'launchctl bootout $LAUNCHD_SERVICE' by hand — bootout tears down the whole job tree and KILLS in-flight sweeps (they are direct children of the launchd job)."
        else
            echo "The running daemon is still the PRE-update binary. Restart manually with:"
            echo "  $STOP_SCRIPT && $START_SCRIPT ${RESTART_ARGS[*]:-}"
        fi
    fi
    exit 0
fi

if [[ "$WAS_RUNNING" != "true" ]]; then
    ok "Rebuilt + provisioned. loom-daemon was not running — nothing to restart."
    echo "Start it with: $START_SCRIPT [flags]"
    exit 0
fi

# ---------- launchd-managed restart via the #4077 supervised primitive (#4042) ----------
# The daemon is launchd-supervised, so NEITHER stop.sh+start.sh NOR .daemon.flags
# apply: the plist's ProgramArguments + EnvironmentVariables are the durable
# source of truth. `loom-daemon restart` sends Request::RestartDaemon over the
# IPC socket; the supervised daemon exits 0 and KeepAlive:SuccessfulExit
# relaunches it onto the freshly-provisioned binary with the plist's config.
if [[ "$DAEMON_MANAGER" == "launchd" ]]; then
    echo "loom-daemon is launchd-managed (label ${LAUNCHD_LABEL})."
    echo "Restarting via the supervised restart primitive: $PROVISION_TARGET restart"
    echo "(.daemon.flags is NOT consulted — the plist's EnvironmentVariables carries the equivalent config.)"
    if "$PROVISION_TARGET" restart; then
        ok "loom-daemon restart scheduled — launchd will relaunch it onto the freshly-provisioned binary."
        exit 0
    fi
    # The restart request is served by the RUNNING (old) binary. A pre-#4077
    # daemon has no RestartDaemon handler (and an unsupervised/dead socket also
    # fails), so the request was refused. Refuse loudly rather than claim a
    # half-update success: the fresh binary is provisioned but the OLD one is
    # still running (the #4011 silent-autonomy-loss class this issue closes).
    err "loom-daemon restart FAILED: the running daemon did not accept the restart request."
    err "This is expected on the FIRST roll onto a #4077-capable binary — the currently-running binary predates the 'restart' IPC command (or its socket is dead)."
    err "The freshly-built binary IS provisioned, but the OLD (unsupervised) binary is still running."

    if [[ "$RELAUNCH" == "true" ]]; then
        perform_relaunch "$LAUNCHD_PLIST"
        exit $?
    fi

    daemon_pid_hint=$(launchd_job_pid)
    err ""
    err "To finish the roll, re-render the plist and relaunch under launchd supervision"
    err "(this installs KeepAlive:{SuccessfulExit:true} + LOOM_DAEMON_SUPERVISOR=launchd so"
    err "the NEXT roll can use the supervised path) while preserving the live plist's LOOM_*"
    err "autonomy env — run:"
    err "  loom-daemon-update.sh --relaunch      (or: LOOM_DAEMON_UPDATE_RELAUNCH=1 loom-daemon-update.sh)"
    err ""
    err "WARNING: do NOT 'launchctl bootout $LAUNCHD_SERVICE' by hand to force this."
    err "bootout tears down the whole job tree, and in-flight sweep children are DIRECT"
    err "children of the launchd job, so it TERMINATES every running sweep — stranding"
    err "loom:building labels and leaving worktrees behind. --relaunch above instead stops"
    err "the daemon gracefully (SIGTERM) so sweep children reparent and keep working."
    err "If you must relaunch by hand, prefer the graceful sequence over bootout+bootstrap:"
    err "  kill -TERM ${daemon_pid_hint:-<daemon-pid>}   # daemon exits non-zero; children reparent; not relaunched (stale plist KeepAlive=false)"
    err "  $START_SCRIPT                                  # re-render + bootstrap the supervised plist"
    exit 6
fi

# ---------- PID-file/nohup-managed restart (preserve prior flags exactly) ----------
if [[ "$FLAGS_SOURCE" == "$FLAGS_FILE" ]]; then
    echo "Restarting with the flags persisted at the last start ($FLAGS_FILE): ${RESTART_ARGS[*]:-<none>}"
else
    warn "No $FLAGS_FILE found — restarting FLAGS-OFF (bare) rather than guessing the prior autonomy flags."
fi

echo "Stopping loom-daemon..."
# --restarting preserves the autonomy-desired marker + watchdog across this
# internal stop (#4011): a self-update is NOT operator intent to stop, so the
# detector must NOT be disarmed — otherwise every self-update would silently turn
# off the very autonomy-loss detection this issue adds (the exact bug class it
# fixes). The subsequent start re-writes the marker and re-provisions the watchdog.
if ! "$STOP_SCRIPT" --restarting; then
    err "loom-daemon-stop.sh failed — NOT starting the new binary on top of a still-running old one."
    exit 1
fi

echo "Starting loom-daemon with preserved flags: ${RESTART_ARGS[*]:-<none>}"
# Guard the array expansion: RESTART_ARGS is empty for a bare/FLAGS-OFF
# restart, and "${arr[@]}" on a zero-element array is an unbound variable
# error under `set -u` on bash < 4.4 (still the default /bin/bash on stock
# macOS).
if [[ "${#RESTART_ARGS[@]}" -gt 0 ]]; then
    "$START_SCRIPT" "${RESTART_ARGS[@]}"
else
    "$START_SCRIPT"
fi
