#!/usr/bin/env bash
# guard-uncommitted-work.sh — Stop / SubagentStop entry point for the
# uncommitted-work guard in CONSUMER workspaces, as an opt-in canary (#8372).
#
# THIN STUB. The decision, the opt-in gate and the outcome recording are all
# `loom-daemon worktree-state stop-hook --consumer-canary` (Rust:
# loom-daemon/src/worktree_state/consumer_canary.rs and stop_hook.rs). This file
# exists because a hook needs a shell-invocable command that
# scripts/install/provision-hooks.sh can wire by name.
#
# OPT-IN. The daemon prints nothing and records nothing unless the workspace's
# effective config sets `guards.uncommittedWorkConsumerCanary: true`. The
# existing `guards.uncommittedWork: false` still disables the guard inside an
# opted-in workspace. Rollback: set the canary key to false (or remove it).
# Loom's own repo runs the guard directly from .claude/settings.json (the
# dogfood wiring, unchanged) and must NOT also set the canary key.
#
# CONTRACT: reads the hook payload on stdin, prints the daemon's decision JSON
# (`{"decision":"block",...}` / `{"systemMessage":...}`) or nothing, and ALWAYS
# exits 0. A Stop hook's exit 2 means "block the turn", so a bare `exec` would
# turn clap's usage error from an older daemon into a block on every turn — the
# #8377 wedge. Every failure here is an allow:
#   - the daemon library is missing        -> wrapper_error lib_missing
#   - no daemon binary resolves            -> wrapper_error binary_unresolved
#   - the daemon exits 2 (too old to know `worktree-state` or
#     `--consumer-canary`)                 -> wrapper_error unsupported_subcommand_or_flag
#   - the daemon exits non-zero otherwise  -> wrapper_error daemon_exit_<rc>
# On a non-zero exit any partial stdout is discarded, never forwarded.
#
# WRAPPER-FAILURE RECORDS. When the daemon cannot run, it cannot record
# anything, so this stub appends ONE line itself (`"source":"wrapper"`) to the
# same bounded canary log — but only for a workspace whose config tiers name the
# canary key as true (a cheap text match, used ONLY to decide whether to record;
# the real gate is the daemon's config resolution). These records stay visible
# and are excluded from the detection false-positive denominator. The stub
# never grows the log past its size bound; the daemon owns rotation.
#
# requires-daemon: worktree-state optional   #8372 — an older binary's clap exit 2 is recorded as a wrapper failure and the stop is allowed (never a block).

set -uo pipefail  # NOTE: no -e — a Stop hook must never exit non-zero

PAYLOAD="$(cat 2>/dev/null)" || PAYLOAD=""

# One correlation id per hook invocation, shared with the daemon's record.
LOOM_HOOK_INVOCATION_ID="inv-$(date -u +%Y%m%dT%H%M%SZ 2>/dev/null)-$$-${RANDOM}"
export LOOM_HOOK_INVOCATION_ID

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd 2>/dev/null || echo ".")"

# Workspace root: LOOM_PROJECT_ROOT (set by the user-scope wrapper), then the
# MAIN checkout via git-common-dir (correct from a linked worktree), then
# Claude Code's project dir, then the cwd.
ROOT="${LOOM_PROJECT_ROOT:-}"
if [[ -z "$ROOT" ]]; then
    GIT_COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)" || GIT_COMMON_DIR=""
    [[ -n "$GIT_COMMON_DIR" ]] && ROOT="$(cd "$GIT_COMMON_DIR/.." 2>/dev/null && pwd)"
fi
[[ -n "$ROOT" ]] || ROOT="${CLAUDE_PROJECT_DIR:-$PWD}"

_json_str() { # minimal JSON string escape for paths / ids
    local s="${1//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

_record_wrapper_failure() { # $1 = failure class
    local f opted=0
    for f in "$ROOT/.loom/config.json" "$ROOT/.loom-project/project.json" "$ROOT/.loom-local/local.json"; do
        if [[ -f "$f" ]] && grep -Eq '"uncommittedWorkConsumerCanary"[[:space:]]*:[[:space:]]*true' "$f" 2>/dev/null; then
            opted=1
            break
        fi
    done
    [[ "$opted" == 1 ]] || return 0
    local log="${LOOM_UNCOMMITTED_WORK_CANARY_LOG:-${HOME:-}/.loom/logs/uncommitted-work-canary.jsonl}"
    [[ "$log" == /* ]] || return 0
    mkdir -p "$(dirname "$log")" 2>/dev/null || return 0
    local size=0
    [[ -f "$log" ]] && size="$(wc -c <"$log" 2>/dev/null | tr -d ' ')"
    [[ "${size:-0}" -lt 1048576 ]] || return 0
    local event=""
    if [[ "$PAYLOAD" =~ \"hook_event_name\"[[:space:]]*:[[:space:]]*\"([A-Za-z]+)\" ]]; then
        event="${BASH_REMATCH[1]}"
    fi
    printf '{"schema":1,"ts":"%s","source":"wrapper","invocation_id":"%s","workspace":"%s","event":"%s","outcome":"wrapper_error","error":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)" \
        "$(_json_str "$LOOM_HOOK_INVOCATION_ID")" "$(_json_str "$ROOT")" "$event" "$1" \
        >>"$log" 2>/dev/null || true
}

# At runtime SCRIPT_DIR is .loom/hooks/ (project-copy wiring) or
# defaults/hooks/ (machine-level wiring); ../scripts/lib resolves in both.
LIB="$SCRIPT_DIR/../scripts/lib/locate-daemon-bin.sh"
# shellcheck source=../scripts/lib/locate-daemon-bin.sh
if [[ ! -r "$LIB" ]] || ! source "$LIB" >/dev/null 2>&1; then
    _record_wrapper_failure lib_missing
    exit 0
fi

BIN="$(loom_resolve_self_daemon_bin 2>/dev/null)" || BIN=""
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
    _record_wrapper_failure binary_unresolved
    exit 0
fi

OUT="$(printf '%s' "$PAYLOAD" | "$BIN" worktree-state stop-hook --consumer-canary 2>/dev/null)"
RC=$?
if [[ "$RC" -ne 0 ]]; then
    if [[ "$RC" -eq 2 ]]; then
        _record_wrapper_failure unsupported_subcommand_or_flag
    else
        _record_wrapper_failure "daemon_exit_${RC}"
    fi
    exit 0
fi
[[ -n "$OUT" ]] && printf '%s\n' "$OUT"
exit 0
