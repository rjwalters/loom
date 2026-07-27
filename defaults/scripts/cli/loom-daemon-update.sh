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
# Usage:
#   ./.loom/scripts/cli/loom-daemon-update.sh              Detect, rebuild if stale, provision, restart (preserving flags)
#   ./.loom/scripts/cli/loom-daemon-update.sh --check       Detect only; exit 0 (up to date) or 3 (update available); no writes
#   ./.loom/scripts/cli/loom-daemon-update.sh --dry-run     Print the plan without building/provisioning/restarting
#   ./.loom/scripts/cli/loom-daemon-update.sh --force       Rebuild + provision + restart even if already up to date
#   ./.loom/scripts/cli/loom-daemon-update.sh --no-restart  Rebuild + provision only; leave the running daemon untouched
#   ./.loom/scripts/cli/loom-daemon-update.sh --help
#
# Environment:
#   LOOM_DAEMON_BIN       Path to the loom-daemon binary (else auto-detected,
#                          same resolution as loom-daemon-start.sh). When set,
#                          the fresh binary is provisioned directly to this
#                          exact path instead of the machine-level default.
#   LOOM_DAEMON_BIN_DIR   Machine-level install dir (default ~/.local/bin),
#                          forwarded to provision-daemon.sh.
#
# Exit codes:
#   0  up to date (no-op) OR rebuild+provision+restart succeeded
#   1  usage error / not a source checkout / build or provision failure
#   3  (--check only) update available
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

# ---------- args ----------
DRY_RUN=false
FORCE=false
CHECK_ONLY=false
NO_RESTART=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h) show_help; exit 0 ;;
        --dry-run) DRY_RUN=true; shift ;;
        --force) FORCE=true; shift ;;
        --check) CHECK_ONLY=true; shift ;;
        --no-restart) NO_RESTART=true; shift ;;
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

# ---------- --check: report only, no writes ----------
if [[ "$CHECK_ONLY" == "true" ]]; then
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
WAS_RUNNING=false
if [[ -f "$PID_FILE" ]]; then
    existing_pid=$(cat "$PID_FILE" 2>/dev/null || true)
    if [[ -n "$existing_pid" ]] && kill -0 "$existing_pid" 2>/dev/null; then
        WAS_RUNNING=true
    fi
fi

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
else
    # shellcheck disable=SC1091
    if [[ -r "$REPO_ROOT/scripts/install/provision-daemon.sh" ]]; then
        source "$REPO_ROOT/scripts/install/provision-daemon.sh"
    fi
    if declare -F provision_machine_daemon >/dev/null 2>&1; then
        if ! provision_machine_daemon "$NEW_BIN"; then
            warn "Machine-level provisioning reported an issue (see above); the freshly-built binary is still available at $NEW_BIN"
        fi
    else
        warn "scripts/install/provision-daemon.sh not found/sourceable — skipping machine-level provisioning."
        warn "Freshly-built binary: $NEW_BIN (set LOOM_DAEMON_BIN=$NEW_BIN to use it directly)"
    fi
fi

# ---------- restart (preserve prior flags exactly — Issue #3968) ----------
if [[ "$NO_RESTART" == "true" ]]; then
    ok "Rebuilt + provisioned. Skipping restart (--no-restart)."
    if [[ "$WAS_RUNNING" == "true" ]]; then
        echo "The running daemon is still the PRE-update binary. Restart manually with:"
        echo "  $STOP_SCRIPT && $START_SCRIPT ${RESTART_ARGS[*]:-}"
    fi
    exit 0
fi

if [[ "$WAS_RUNNING" != "true" ]]; then
    ok "Rebuilt + provisioned. loom-daemon was not running — nothing to restart."
    echo "Start it with: $START_SCRIPT [flags]"
    exit 0
fi

if [[ "$FLAGS_SOURCE" == "$FLAGS_FILE" ]]; then
    echo "Restarting with the flags persisted at the last start ($FLAGS_FILE): ${RESTART_ARGS[*]:-<none>}"
else
    warn "No $FLAGS_FILE found — restarting FLAGS-OFF (bare) rather than guessing the prior autonomy flags."
fi

echo "Stopping loom-daemon..."
if ! "$STOP_SCRIPT"; then
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
