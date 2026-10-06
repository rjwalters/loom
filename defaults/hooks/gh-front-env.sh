#!/usr/bin/env bash
# gh-front-env.sh — SessionStart hook: put the agent `gh` front first on the
# session's PATH (issue #10516).
#
# THIN STUB. The logic is `loom-daemon gh-shim session-env`
# (`loom-daemon/src/agent_gh/session_env.rs`): it appends one guarded PATH line
# to `$CLAUDE_ENV_FILE`, which Claude Code sources before every Bash tool call
# in the session AND in its Task subagents, so their plain `gh` reads are
# ETag-revalidated like a dispatched worker's (#10331). Order is the worker's:
# managed launcher (#9987) when a policy names one, then the front, then the
# PATH the session already had. Opt out with LOOM_GH_SHIM=0.
#
# CONTRACT: a cost optimisation, never a guard. Every failure is exit 0, and
# nothing reaches stdout (SessionStart stdout is injected into the model's
# context). Diagnostics go to stderr.

set -uo pipefail  # NOTE: no -e — this hook must never fail a session

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd 2>/dev/null || echo ".")"

ROOT="${LOOM_PROJECT_ROOT:-}"
if [[ -z "$ROOT" ]]; then
    GIT_COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)" || GIT_COMMON_DIR=""
    [[ -n "$GIT_COMMON_DIR" ]] && ROOT="$(cd "$GIT_COMMON_DIR/.." 2>/dev/null && pwd)"
fi
[[ -n "$ROOT" ]] || ROOT="${CLAUDE_PROJECT_DIR:-$PWD}"

LIB="$SCRIPT_DIR/../scripts/lib/locate-daemon-bin.sh"
[[ -r "$LIB" ]] || exit 0
# shellcheck source=../scripts/lib/locate-daemon-bin.sh
source "$LIB" >/dev/null 2>&1 || exit 0
BIN="$(loom_daemon_self_bin_override || loom_locate_daemon_bin "$ROOT")" 2>/dev/null
[[ -n "$BIN" && -x "$BIN" ]] || exit 0

# requires-daemon: gh-shim optional   #10516 — an older binary prints gh-shim usage to stderr and exits 2; the hook still exits 0 and the session keeps its PATH.
LOOM_PROJECT_ROOT="$ROOT" "$BIN" gh-shim session-env 1>&2
exit 0
