#!/usr/bin/env bash
# spawn-worker.sh - Runtime-dispatch seam in front of the worker spawn path.
#
# This thin dispatcher is the single indirection point that turns "the worker
# is always Claude Code" into "the worker is whichever runtime adapter the
# operator selected". Claude Code is adapter #1 (`spawn-claude.sh`); a future
# Codex adapter slots in as `spawn-codex.sh` behind the same seam with no
# caller change. It mirrors the multi-runtime fork's `spawn-worker.sh` so the
# two trees converge (epic #4167, Phase 1).
#
# Zero behavior change: with no `LOOM_RUNTIME` env and no `runtimes.default`
# in `.loom/config.json`, this execs `spawn-claude.sh` with the args
# verbatim — byte-for-byte the same worker Loom spawned before this seam
# existed. Existing callers that invoke `spawn-claude.sh` directly are
# unaffected; migrating them to `spawn-worker.sh` is a follow-up.
#
# Runtime resolution (standard precedence, highest first):
#   1. LOOM_RUNTIME env var (non-empty).
#   2. `.loom/config.json` -> runtimes.default.
#   3. Built-in default: "claude".
#
# Dispatch: exec "$SCRIPT_DIR/spawn-<runtime>.sh" "$@" — all args forwarded
# verbatim, and the runner's exit code is passed through by exec.
#
# Failure: an unknown runtime (no matching `spawn-<runtime>.sh` on disk) exits
# 78 (EX_CONFIG) with a message naming the resolved runtime, where it was
# resolved from (env vs config vs default), and the runners actually present.
#
# Sweep/role-runner scheduling priority (issue #4233): applied inside
# `spawn-<runtime>.sh` (currently only `spawn-claude.sh`), NOT here — this
# dispatcher only ever `exec`s into a runner, which preserves the pid, so a
# runner-level `nice`/`taskpolicy` re-exec still covers every process this
# script itself would otherwise have spawned. Deliberately not duplicated at
# this layer to avoid a double-apply; a future `spawn-<runtime>.sh` adopting a
# new runtime should apply its own priority policy the same way
# `spawn-claude.sh` does, not rely on this dispatcher for it.
#
# Usage:
#   .loom/scripts/spawn-worker.sh -p "your prompt"
#   LOOM_RUNTIME=claude .loom/scripts/spawn-worker.sh --use-wrapper -p "..."
#
# Test-isolation defaults (issue #8077):
#
# A worker spawned by the daemon inherits the DAEMON's environment, and the
# daemon's own systemd unit sets `LOOM_SOCKET_PATH=$HOME/.loom/loom-daemon.sock`.
# `resolve_loom_dir()` takes that variable's PARENT as the daemon's loom dir, so
# the "default" any `loom-daemon` a worker spawns resolves is not a neutral one
# — it is the LIVE production `~/.loom`. A test that merely omits an override
# therefore writes into the operator's real `daemon.log`, reads the real
# workspace registry, and reconciles against the real machine sweep journal.
# That is not hypothetical: on 2026-09-17 a builder sweep on loom-worker-2 put
# 17 daemon boot blocks into the production log and had one of those daemons
# adopt the live host's in-flight sweep claim.
#
# The seam every sweep worker passes through is the right place to make the
# default safe, because a Builder cannot forget what it never had to remember.
# Both are set with `${VAR:-default}` semantics — an explicit caller value always
# wins, so a deliberate operator override is preserved.
#
#   LOOM_DAEMON_LOG       A per-worker scratch log path. This is the daemon's
#                         HIGHEST-precedence log tier (`resolve_log_path`, ahead
#                         of the LOOM_SOCKET_PATH-derived default), so a daemon
#                         spawned anywhere under this worker logs there instead
#                         of into production. Deliberately the ONLY path var
#                         repointed here: LOOM_SOCKET_PATH, LOOM_WORKSPACE and
#                         LOOM_SHARED_TOKENS_DIR must keep their production
#                         values, because the worker itself legitimately talks
#                         to the real daemon and draws from the real token pool
#                         (repointing them would break the sweep, not isolate
#                         it). Test-side isolation for those lives in
#                         defaults/scripts/tests/lib/live-state-sandbox.sh.
#   LOOM_TEST_ALLOW_SYSTEMD  `0`. Blocks that drive the LIVE `systemctl --user`
#                         manager are opt-in; inside a sweep, that manager is
#                         the one supervising the production daemon, so the
#                         answer is always no. CI opts in explicitly instead.
#
# Env vars:
#   LOOM_RUNTIME     Selects the runtime adapter (highest precedence). An empty
#                    value is treated as unset (falls through to config/default).
#   LOOM_WORKSPACE   Override repo-root detection (used only to locate
#                    `.loom/config.json`).

set -euo pipefail

# --- Logging helpers (match loom convention) ---
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')]${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] WARN${NC} $*" >&2; }
log_error() { echo -e "${RED}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR${NC} $*" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- Test-isolation defaults (#8077) ---
# See the header block for why these belong here rather than in each test. The
# scratch log path is per-worker (pid-suffixed) so two concurrent workers on the
# same host cannot interleave into one file, and lives under $TMPDIR so it is
# reaped with the rest of the host's temp state rather than accumulating inside
# a repo. Deliberately NOT `mkdir`ed here: `setup_logging()` already does
# `create_dir_all(log_path.parent())` before opening the file, so the directory
# materializes only if a daemon is actually spawned — a worker that never
# spawns one (the overwhelming majority) leaves nothing behind at all.
export LOOM_DAEMON_LOG="${LOOM_DAEMON_LOG:-${TMPDIR:-/tmp}/loom-worker-isolation-$$/daemon.log}" LOOM_TEST_ALLOW_SYSTEMD="${LOOM_TEST_ALLOW_SYSTEMD:-0}"

# --- Repo root resolution (handles worktrees) ---
# Only used to locate `.loom/config.json`. Mirrors spawn-claude.sh:
#   1. Trust LOOM_WORKSPACE if set.
#   2. Else derive from `git rev-parse --git-common-dir` (works in worktrees).
#   3. Else fall back relative to this script (.loom/scripts -> repo root).
_resolve_workspace() {
    if [[ -n "${LOOM_WORKSPACE:-}" ]]; then
        printf '%s\n' "$LOOM_WORKSPACE"
        return
    fi

    local git_common_dir
    if git_common_dir="$(git rev-parse --git-common-dir 2>/dev/null)"; then
        if [[ ! "$git_common_dir" = /* ]]; then
            git_common_dir="$(cd "$git_common_dir" && pwd)"
        fi
        printf '%s\n' "$(dirname "$git_common_dir")"
        return
    fi

    cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
}

# --- List the spawn-<runtime>.sh runners present on disk (for messages) ---
# Echoes a space-separated list of runtime names (the `<runtime>` slug), with
# the dispatcher itself (`worker`) excluded.
_available_runtimes() {
    local f name
    local found=()
    for f in "$SCRIPT_DIR"/spawn-*.sh; do
        [[ -e "$f" ]] || continue
        name="$(basename "$f")"
        name="${name#spawn-}"
        name="${name%.sh}"
        [[ "$name" == "worker" ]] && continue
        found+=("$name")
    done
    printf '%s' "${found[*]:-<none>}"
}

# --- Resolve the runtime (env > config > default) ---
RUNTIME="" RUNTIME_SOURCE=""

if [[ -n "${LOOM_RUNTIME:-}" ]]; then
    RUNTIME="$LOOM_RUNTIME"
    RUNTIME_SOURCE="env (LOOM_RUNTIME)"
else
    _cfg_runtime=""
    _resolver_lib="$SCRIPT_DIR/lib/config-resolver.sh"
    if [[ -f "$_resolver_lib" ]]; then
        # shellcheck source=./lib/config-resolver.sh
        source "$_resolver_lib"
        _workspace="$(_resolve_workspace)"
        # loom_config_get soft-fails to the default (here "") on a missing
        # config file, a missing `runtimes` block, or a missing `jq` — so a
        # bare install with no runtimes config resolves to "claude" below.
        _cfg_runtime="$(loom_config_get "$_workspace" "runtimes.default" "")"
    fi

    if [[ -n "$_cfg_runtime" ]]; then
        RUNTIME="$_cfg_runtime"
        RUNTIME_SOURCE="config (runtimes.default)"
    else
        RUNTIME="claude"
        RUNTIME_SOURCE="default"
    fi
fi

# --- Dispatch ---
RUNNER="$SCRIPT_DIR/spawn-$RUNTIME.sh"
if [[ ! -f "$RUNNER" ]]; then
    log_error "Unknown runtime '$RUNTIME' (resolved from $RUNTIME_SOURCE):"
    log_error "no runner found at $RUNNER."
    log_error "Available runtimes on disk: $(_available_runtimes)."
    log_error "Set LOOM_RUNTIME, or .loom/config.json -> runtimes.default, to a"
    log_error "runtime that has a matching spawn-<runtime>.sh runner."
    exit 78  # EX_CONFIG
fi

log_info "spawn-worker: runtime=$RUNTIME (from $RUNTIME_SOURCE) -> $(basename "$RUNNER")"
echo "# LOOM_RUNTIME_RESOLVED runtime=$RUNTIME" >&2
exec "$RUNNER" "$@"
