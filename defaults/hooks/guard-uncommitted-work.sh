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
# same bounded canary log — but only for a workspace whose effective config sets the
# canary key to true (a cheap text match, used ONLY to decide whether to record;
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
[[ -n "$ROOT" ]] || ROOT="$(cd "$(git rev-parse --git-common-dir 2>/dev/null || echo /nonexistent)/.." 2>/dev/null && pwd)"
[[ -n "$ROOT" ]] || ROOT="${CLAUDE_PROJECT_DIR:-$PWD}"

# Effective value of the canary key across the config tiers, lowest to highest
# precedence (private defaults < .loom/config.json < .loom-project/project.json
# < .loom-local/local.json — config_resolver.rs): the LAST tier naming the key
# as a boolean wins, so a higher-tier false suppresses wrapper records. Returns
# 0 only when the effective value is true. A text match, not a JSON parse.
_canary_effective_true() {
    local f v eff="" defaults="${LOOM_CONFIG_DEFAULTS_FILE-${HOME:-}/.local/share/loom/config/defaults.json}"
    for f in "$defaults" "$ROOT/.loom/config.json" "$ROOT/.loom-project/project.json" "$ROOT/.loom-local/local.json"; do
        [[ -n "$f" && -r "$f" ]] || continue
        v="$(grep -Eso '"uncommittedWorkConsumerCanary"[[:space:]]*:[[:space:]]*(true|false)' "$f" 2>/dev/null | tail -n1)"
        [[ -n "$v" ]] && eff="${v##*[: ]}"
    done
    [[ "$eff" == true ]]
}

# $1 = failure class. Records only for a workspace whose EFFECTIVE canary key is
# true (see _canary_effective_true), and never grows the log past its 1 MiB bound (the daemon owns rotation).
_record_wrapper_failure() {
    _canary_effective_true || return 0
    local log="${LOOM_UNCOMMITTED_WORK_CANARY_LOG:-${HOME:-}/.loom/logs/uncommitted-work-canary.jsonl}" event="" ws
    [[ "$log" == /* ]] && mkdir -p "$(dirname "$log")" 2>/dev/null || return 0
    [[ "$(wc -c 2>/dev/null <"$log")" -lt 1048576 ]] || return 0  # missing log -> "" -> 0
    [[ "$PAYLOAD" =~ \"hook_event_name\"[[:space:]]*:[[:space:]]*\"([A-Za-z]+)\" ]] && event="${BASH_REMATCH[1]}"
    ws="${ROOT//\\/\\\\}" && ws="${ws//\"/\\\"}"  # minimal JSON string escape
    printf '{"schema":1,"ts":"%s","source":"wrapper","invocation_id":"%s","workspace":"%s","event":"%s","outcome":"wrapper_error","error":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)" "$LOOM_HOOK_INVOCATION_ID" "$ws" "$event" "$1" >>"$log" 2>/dev/null || true
}

# At runtime SCRIPT_DIR is .loom/hooks/ (project-copy wiring) or
# defaults/hooks/ (machine-level wiring); ../scripts/lib resolves in both.
LIB="$SCRIPT_DIR/../scripts/lib/locate-daemon-bin.sh"
# shellcheck source=../scripts/lib/locate-daemon-bin.sh
{ [[ -r "$LIB" ]] && source "$LIB" >/dev/null 2>&1; } || { _record_wrapper_failure lib_missing; exit 0; }
BIN="$(loom_resolve_self_daemon_bin 2>/dev/null)" || BIN=""
[[ -n "$BIN" && -x "$BIN" ]] || { _record_wrapper_failure binary_unresolved; exit 0; }

# loom-daemon call-site. Any non-zero exit discards the partial stdout.
OUT="$(printf '%s' "$PAYLOAD" | "$BIN" worktree-state stop-hook --consumer-canary 2>/dev/null)"
RC=$?
if [[ "$RC" -ne 0 ]]; then
    [[ "$RC" -eq 2 ]] && CLASS=unsupported_subcommand_or_flag || CLASS="daemon_exit_${RC}"
    _record_wrapper_failure "$CLASS"
    exit 0
fi
[[ -n "$OUT" ]] && printf '%s\n' "$OUT"
exit 0
