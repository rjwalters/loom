#!/usr/bin/env bash
# spawn-claude.sh - Token-rotating launcher for Claude Code.
#
# This script is a thin layer that:
#   1. Selects a Claude Code OAuth token from .loom/tokens/ via
#      `python3 -m loom_tools.tokens.select`.
#   2. Exports CLAUDE_CODE_OAUTH_TOKEN.
#   3. exec's the underlying CLI (`claude` by default, or
#      `claude-wrapper.sh` if --use-wrapper is passed for retry behavior).
#
# It does NOT replace the existing 1700-LOC `.loom/scripts/claude-wrapper.sh`,
# which provides retry, backoff, auth-cache, and error classification.
# Use `claude-wrapper.sh` directly when you need that behavior; use this
# script when you want pure token rotation in front of either `claude` or
# the wrapper.
#
# Behavior on missing tokens:
#   Token selection resolves the effective pool (issue #3938): the per-repo
#   pool at `<repo>/.loom/tokens/` when it holds `*.token` files, else the
#   shared machine-level pool at `~/.loom/tokens/` (override
#   `LOOM_SHARED_TOKENS_DIR`; set it empty to disable the fallback). This lets a
#   consumer repo the daemon dispatches into — which has no pool of its own —
#   spawn against the shared pool instead of hard-failing. All pool STATE
#   (`.bad_tokens`/`.ranking`/`.allowlist`/`.failure_counts`) lives in whichever
#   pool was selected, so it is never forked per repo.
#   When NEITHER pool exists/has tokens (or all tokens are bad), this script
#   exits 78 (EX_CONFIG) with a message instructing the user to run
#   `loom-tokens bootstrap` (or `loom-tokens bootstrap --shared` for the
#   machine-level pool). It does NOT silently fall back to keychain.
#
# Worktree handling:
#   When invoked from a git worktree, the script resolves the canonical repo
#   root via `git rev-parse --git-common-dir` and looks up `.loom/tokens/`
#   there — never in the worktree's path.
#
# Env vars (pool location):
#   LOOM_SHARED_TOKENS_DIR  Shared machine-level pool location. Non-empty path
#                           overrides the `~/.loom/tokens` default; an empty
#                           value disables the shared fallback (per-repo only).
#
# Usage:
#   .loom/scripts/spawn-claude.sh -p "your prompt"
#   .loom/scripts/spawn-claude.sh --use-wrapper --prompt "..." --log /tmp/log
#
# Env vars:
#   LOOM_WORKSPACE         Override repo root detection.
#   LOOM_SPAWN_NO_EXPORT   If set, skip selection (caller already exported a
#                          token). Useful for testing the dispatch path.
#   LOOM_PYTHON            Override the python interpreter (default: python3).
#   LOOM_MODEL             Model to pass as `claude --model <value>` (issue
#                          #3477). Lowest-priority tier: an explicit `--model`
#                          in the passthrough args always wins. When neither
#                          is set, NO --model flag is emitted and the session/
#                          CLI default is preserved.
#   LOOM_EFFORT            Reasoning-effort level to pass as `claude --effort
#                          <value>` (issue #3705). Mirrors LOOM_MODEL: an
#                          explicit `--effort` in the passthrough args wins;
#                          when neither is set, NO --effort flag is emitted and
#                          the session/CLI default is preserved. This sets a
#                          session-default effort for the whole spawned child —
#                          the sweep escalation ladder's per-rung `@effort`
#                          cannot be threaded here because the in-session Task
#                          tool exposes no effort parameter (see sweep.md).

set -euo pipefail

# --- Logging helpers (match loom convention) ---
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')]${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] WARN${NC} $*" >&2; }
log_error() { echo -e "${RED}[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ERROR${NC} $*" >&2; }

# --- Repo root resolution (handles worktrees) ---
# If LOOM_WORKSPACE is set, trust it. Otherwise:
#   1. Try `git rev-parse --git-common-dir` to find the canonical .git dir
#      (works inside main checkouts and worktrees alike).
#   2. The parent of `.git` (or of the common dir if it's not literally .git)
#      is the canonical repo root.
#   3. Fallback: walk up from the script's directory.
_resolve_workspace() {
    if [[ -n "${LOOM_WORKSPACE:-}" ]]; then
        printf '%s\n' "$LOOM_WORKSPACE"
        return
    fi

    local git_common_dir
    if git_common_dir="$(git rev-parse --git-common-dir 2>/dev/null)"; then
        # `git rev-parse --git-common-dir` may return a relative path inside
        # a worktree — convert to absolute, then take parent.
        if [[ ! "$git_common_dir" = /* ]]; then
            git_common_dir="$(cd "$git_common_dir" && pwd)"
        fi
        # If common-dir basename is `.git`, parent is repo root.
        # Otherwise (linked worktree case), it's the literal main `.git/`
        # directory — its parent is still the canonical main checkout.
        printf '%s\n' "$(dirname "$git_common_dir")"
        return
    fi

    # Fallback: relative to this script
    cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
}

WORKSPACE="$(_resolve_workspace)"
PYTHON="${LOOM_PYTHON:-python3}"

# --- Daemon self-claim marker visibility (Issue #3823, observability #3967) ---
# `loom-daemon`'s `SweepRegistry::spawn_child` exports
# `LOOM_SWEEP_CLAIM_OWNED=<issue>` into a daemon-dispatched child so its
# `/loom:sweep` pre-flight recognises the daemon's own pre-dispatch
# `loom:issue -> loom:building` label flip as ITS OWN claim rather than
# self-skipping the issue as "another worker is in flight" (the #3967
# incident). Log the marker's presence/value here — unconditionally, on
# every spawn, whether daemon-dispatched or not — so a future occurrence is
# diagnosable straight from `.loom/logs/sweep-issue-<N>.log` without needing
# to reproduce the dispatch: this line, plus the `spawn-claude:` prefix, is
# already captured by `spawn_child`'s per-sweep log redirection, so any gap
# between the Rust `Command::env()` call and what this script's own
# environment actually sees is now directly observable instead of inferred.
log_info "spawn-claude: LOOM_SWEEP_CLAIM_OWNED=${LOOM_SWEEP_CLAIM_OWNED:-unset}"

# --- Argument parsing ---
USE_WRAPPER=false
PASSTHROUGH_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --use-wrapper)
            USE_WRAPPER=true
            shift
            ;;
        --help|-h)
            sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' \
                | head -n -1
            exit 0
            ;;
        --)
            shift
            PASSTHROUGH_ARGS+=("$@")
            break
            ;;
        *)
            PASSTHROUGH_ARGS+=("$1")
            shift
            ;;
    esac
done

# --- Model selection (issue #3477, Phase 1; observability #3482, Phase 3a) ---
# Precedence: explicit `--model` in the passthrough args > LOOM_MODEL env >
# nothing (session/CLI default — no --model flag emitted at all).
#
# Observability (#3482): exactly ONE structured `spawn-claude: model=<value>`
# line is emitted on every spawn, covering all three precedence cases —
# including `model=default` when nothing is configured. The line is
# stderr-only and changes NO spawn behavior; downstream log scrapers key on
# the `model=` token.
_explicit_model=""
_has_model_arg=false
_prev_was_model_flag=false
for _arg in ${PASSTHROUGH_ARGS[@]+"${PASSTHROUGH_ARGS[@]}"}; do
    if [[ "$_prev_was_model_flag" == "true" ]]; then
        _explicit_model="$_arg"
        _prev_was_model_flag=false
        continue
    fi
    case "$_arg" in
        --model)
            _has_model_arg=true
            _prev_was_model_flag=true
            ;;
        --model=*)
            _has_model_arg=true
            _explicit_model="${_arg#--model=}"
            ;;
    esac
done

if [[ "$_has_model_arg" == "true" ]]; then
    if [[ -n "${LOOM_MODEL:-}" ]]; then
        log_info "spawn-claude: explicit --model in args wins over LOOM_MODEL='$LOOM_MODEL'"
    fi
    log_info "spawn-claude: model=${_explicit_model:-default} (from --model arg)"
elif [[ -n "${LOOM_MODEL:-}" ]]; then
    PASSTHROUGH_ARGS+=(--model "$LOOM_MODEL")
    log_info "spawn-claude: model=$LOOM_MODEL (from LOOM_MODEL)"
else
    log_info "spawn-claude: model=default"
fi

# --- Effort selection (issue #3705) ---
# Mirrors the model block above for the `claude --effort <level>` session knob.
# Precedence: explicit `--effort` in the passthrough args > LOOM_EFFORT env >
# nothing (session/CLI default — no --effort flag emitted at all).
#
# This is the ONLY per-call reasoning-effort surface available in this
# environment: the `claude` CLI exposes `--effort`, but the in-session Task
# tool (the sweep's per-role subagent dispatch, one level deep) exposes NO
# effort parameter. So spawn-claude/daemon can set a *session-default* effort
# for a whole `/loom:sweep` child; the escalation ladder's per-rung `@effort`
# still degrades to the bare model at Task-tool dispatch time (see
# `sweep.md` → "Effort graceful degradation").
#
# Observability: exactly ONE structured `spawn-claude: effort=<value>` line
# is emitted only when an effort is actually resolved (explicit arg or env).
# When nothing is configured, NO effort line is emitted and NO --effort flag
# is appended — byte-for-byte identical to pre-#3705 behavior.
_explicit_effort=""
_has_effort_arg=false
_prev_was_effort_flag=false
for _arg in ${PASSTHROUGH_ARGS[@]+"${PASSTHROUGH_ARGS[@]}"}; do
    if [[ "$_prev_was_effort_flag" == "true" ]]; then
        _explicit_effort="$_arg"
        _prev_was_effort_flag=false
        continue
    fi
    case "$_arg" in
        --effort)
            _has_effort_arg=true
            _prev_was_effort_flag=true
            ;;
        --effort=*)
            _has_effort_arg=true
            _explicit_effort="${_arg#--effort=}"
            ;;
    esac
done

if [[ "$_has_effort_arg" == "true" ]]; then
    if [[ -n "${LOOM_EFFORT:-}" ]]; then
        log_info "spawn-claude: explicit --effort in args wins over LOOM_EFFORT='$LOOM_EFFORT'"
    fi
    log_info "spawn-claude: effort=${_explicit_effort:-default} (from --effort arg)"
elif [[ -n "${LOOM_EFFORT:-}" ]]; then
    PASSTHROUGH_ARGS+=(--effort "$LOOM_EFFORT")
    log_info "spawn-claude: effort=$LOOM_EFFORT (from LOOM_EFFORT)"
fi

# --- Locate loom_tools package source ---
# Search order:
#   1. $LOOM_PACKAGE_PATH (env override).
#   2. Script-relative: .loom/scripts/spawn-claude.sh -> ../../loom-tools/src
#      (matches the loom repo layout regardless of WORKSPACE override).
#   3. $WORKSPACE/loom-tools/src.
#
# Tiers 2/3 only resolve inside an actual loom checkout — a CONSUMER repo's
# installed .loom/scripts/spawn-claude.sh has no loom-tools/ sibling, so
# tier 1 is load-bearing there. Issue #3949: `loom-daemon`'s `spawn_child`
# (loom-daemon/src/sweep_registry.rs::resolve_package_path_env) now sets
# LOOM_PACKAGE_PATH automatically on every daemon-dispatched child — derived
# from the loom checkout the running daemon binary was built from — so this
# no longer needs to be exported manually before starting the daemon.
_script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_script_relative_pkg="$(cd "$_script_dir/../../loom-tools/src" 2>/dev/null && pwd || echo "")"
PACKAGE_PATH="${LOOM_PACKAGE_PATH:-$_script_relative_pkg}"
if [[ -z "$PACKAGE_PATH" || ! -d "$PACKAGE_PATH/loom_tools/tokens" ]]; then
    PACKAGE_PATH="${WORKSPACE}/loom-tools/src"
fi

# --- Token selection ---
if [[ -z "${LOOM_SPAWN_NO_EXPORT:-}" && -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
    # Pre-flight: auto-unpin if every allowlisted account has hit the
    # consecutive-failure threshold (default 5). Without this, an
    # operator-set pin can trap the spawner once all pinned accounts
    # exhaust their weekly quota. Empty-pool guard: we never silently
    # clear .bad_tokens — if that file blocks every account, the user
    # must intervene (e.g. `loom-tokens unblock <name>`).
    PYTHONPATH="${PACKAGE_PATH}${PYTHONPATH:+:$PYTHONPATH}" \
        "$PYTHON" - "$WORKSPACE" <<'PY' || true
import sys
from pathlib import Path
try:
    from loom_tools.tokens import allowlist as a
    from loom_tools.tokens import failure_counts as fc
except Exception:
    sys.exit(0)
ws = Path(sys.argv[1])
try:
    pinned = a.read_allowlist(ws)
    if not pinned:
        sys.exit(0)
    if all(fc.threshold_reached(ws, n) for n in pinned):
        a.clear_allowlist(ws)
        fc.reset_all(ws)
        print(
            f"[auto-unpin] All {len(pinned)} pinned account(s) hit "
            f"{fc.DEFAULT_THRESHOLD} consecutive failures; "
            f"cleared .allowlist.",
            file=sys.stderr,
        )
except Exception as exc:  # noqa: BLE001
    print(f"[auto-unpin] skipped ({exc!r})", file=sys.stderr)
PY

    # Capture stdout (JSON) and stderr (errors) separately so log output
    # does not contaminate the JSON we feed to python -c.
    _selection_stderr_file="$(mktemp)"
    _selection_json=""
    if ! _selection_json="$(
        PYTHONPATH="${PACKAGE_PATH}${PYTHONPATH:+:$PYTHONPATH}" \
        "$PYTHON" -m loom_tools.tokens.select --workspace "$WORKSPACE" --json \
        2>"$_selection_stderr_file"
    )"; then
        log_error "Token selection failed:"
        cat "$_selection_stderr_file" >&2 || true
        rm -f "$_selection_stderr_file"
        log_error "Run 'loom-tokens bootstrap' to populate <repo>/.loom/tokens/,"
        log_error "or 'loom-tokens bootstrap --shared' for the machine-level pool"
        log_error "(~/.loom/tokens, override LOOM_SHARED_TOKENS_DIR) that consumer"
        log_error "repos fall back to. Use 'loom-tokens unblock <name>' if"
        log_error ".bad_tokens is the cause."
        log_error "Spawn-claude refuses to auto-clear .bad_tokens — that's"
        log_error "intentional: an empty pool indicates a real auth problem."
        log_error "Set CLAUDE_CODE_OAUTH_TOKEN explicitly to bypass selection."
        exit 78  # EX_CONFIG
    fi
    rm -f "$_selection_stderr_file"

    # Parse JSON without jq (jq isn't guaranteed to be installed).
    _token=$(
        printf '%s' "$_selection_json" \
        | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["key"])'
    )
    _name=$(
        printf '%s' "$_selection_json" \
        | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["name"])'
    )
    _mode=$(
        printf '%s' "$_selection_json" \
        | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["mode"])'
    )

    if [[ -z "$_token" ]]; then
        log_error "Token selection returned empty key for account '$_name'."
        exit 78
    fi

    export CLAUDE_CODE_OAUTH_TOKEN="$_token"
    # Export the selected account name so a downstream claude-wrapper.sh knows
    # which account the exported token belongs to. This is what lets the
    # wrapper mark exactly the right account bad when it rotates on a
    # usage/session-limit fault (issue #3738) instead of guessing from file
    # mtimes. Harmless for the direct-`claude` dispatch path.
    export LOOM_TOKEN_NAME="$_name"
    log_info "spawn-claude: using OAuth account '$_name' (mode=$_mode)"
fi

# --- Print-mode background-task wait ceiling (issue #3943) ---
# When the daemon spawns a sweep child as a headless `claude -p "/loom:sweep N"`
# session, the Claude Code harness (print mode) terminates still-running
# background tasks — the sweep's dispatched Builder/Judge subagents — after a
# 600s ceiling and exits the session. Any role phase taking >10 minutes is
# killed mid-build, causing loom:building<->loom:issue label ping-pong. Disable
# the ceiling (0 = no cap) so long-running phases run to completion. This is
# harmless for interactive use (no background-task reaping there). Use the
# `:=` default-assignment idiom so an operator override — e.g. a non-zero
# ceiling exported in the environment — is preserved.
: "${CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS:=0}"
export CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS

# --- Dispatch ---
if [[ "$USE_WRAPPER" == "true" ]]; then
    _wrapper="${WORKSPACE}/.loom/scripts/claude-wrapper.sh"
    if [[ ! -x "$_wrapper" ]]; then
        log_error "Cannot find executable claude-wrapper.sh at $_wrapper"
        exit 1
    fi
    exec "$_wrapper" "${PASSTHROUGH_ARGS[@]}"
fi

# Default: exec the `claude` CLI directly.
if ! command -v claude >/dev/null 2>&1; then
    log_error "'claude' command not found in PATH."
    log_error "Install Claude Code or pass --use-wrapper to invoke claude-wrapper.sh."
    exit 127
fi
exec claude "${PASSTHROUGH_ARGS[@]}"
