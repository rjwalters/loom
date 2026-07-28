#!/usr/bin/env bash
# guard-worktree-paths.sh - PreToolUse hook to confine Edit/Write to worktree
#
# Blocks Edit and Write tool calls whose file_path resolves outside a
# builder's issue worktree. This prevents builders from escaping their
# worktree and modifying files in the main repository (see issue #2441,
# #4007).
#
# Two independent mechanisms, tried in order:
#
#   1. Env fast path (LOOM_WORKTREE_PATH): when set, only that exact
#      worktree is allowed. Set by tmux/manual sessions that pin one process
#      to one worktree. This is unchanged from the original hook.
#
#   2. Path-derived fallback (no LOOM_WORKTREE_PATH): a daemon-dispatched
#      sweep hosts multiple Task-subagent builders (different issues) in one
#      shared process env, so a single process-wide LOOM_WORKTREE_PATH
#      cannot work there (#3719) -- nothing sets it on that path and the
#      hook was structurally inert. Instead, derive the answer from the
#      target path itself: walk up from the resolved target looking for the
#      `.loom-managed` sentinel that `worktree.sh` writes at the root of
#      every worktree it creates (and never at the main repo root). If the
#      target is inside ANY managed worktree, allow it -- we cannot tell
#      which issue a given subagent owns (no ambient signal exists for
#      that), so this deliberately does not attempt cross-issue isolation.
#      What it DOES catch -- the actual failure mode in #2802 / #3513 /
#      #4007 -- is a write that resolves to the MAIN checkout (typically a
#      repo-relative path evaluated after a cwd reset) while worktree
#      isolation is in play for this repo/session (i.e. at least one
#      managed worktree currently exists). If no managed worktree has ever
#      been created, the hook fails open -- there is nothing to protect.
#
# Toggle: guards.worktreeIsolation (default true) / LOOM_GUARD_WORKTREE_ISOLATION
# env override, following the resolution order used by every other guard
# category in this repo (env > config > default; pattern documented at
# guard-destructive-generic.sh around the guards.sqlDdl toggle).
#
# Input (JSON on stdin):
#   { "tool_input": { "file_path": "/path/to/file", ... }, "cwd": "/cwd" }
#
# Output:
#   Exit 0 with no output = allow
#   Exit 0 with JSON { "hookSpecificOutput": { "permissionDecision": "deny", ... } } = block
#
# Contract: NEVER exits non-zero. Fails open on every unexpected condition
# (missing jq, unparseable input, no sentinel anywhere, resolver errors) so a
# broken guard cannot wedge all agent writes.

# Determine main repo root via git-common-dir (works from worktrees and subdirectories)
MAIN_ROOT="$(cd "$(git rev-parse --git-common-dir 2>/dev/null)/.." 2>/dev/null && pwd)" || \
MAIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." 2>/dev/null && pwd 2>/dev/null || echo ".")"
HOOK_ERROR_LOG="${MAIN_ROOT}/.loom/logs/hook-errors.log"

log_hook_error() {
    mkdir -p "$(dirname "$HOOK_ERROR_LOG")" 2>/dev/null || true
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] [guard-worktree-paths] $1" >> "$HOOK_ERROR_LOG" 2>/dev/null || true
}

# Top-level error trap: on ANY unexpected error, allow to prevent infinite retry loops
trap 'log_hook_error "Unexpected error on line ${LINENO}: ${BASH_COMMAND:-unknown} (exit=$?)"; exit 0' ERR

# =============================================================================
# Guard category toggle — guards.worktreeIsolation / LOOM_GUARD_WORKTREE_ISOLATION
#
# Default ON. Resolution order (highest precedence first):
#   1. LOOM_GUARD_WORKTREE_ISOLATION env var (0/false/no disables, 1/true/yes forces on)
#   2. .loom/config.json  ->  guards.worktreeIsolation  (default true when absent)
#   3. Default: true (guard on)
#
# The config read is best-effort: any parse failure falls through to
# guard-ON and never trips the ERR trap or produces a non-zero exit.
#
# CARVE-OUT (#4241, same class as #4063): this read is deliberately NOT routed
# through the shared config-resolver.sh (loom_config_get / loom_resolve_config)
# that the cold-path guards.* toggles in guard-destructive-generic.sh use.
# worktree_isolation_guard_enabled() is called UNCONDITIONALLY as the very
# first thing this hook does, on EVERY Edit/Write PreToolUse -- there is no
# cheaper structural pre-check upstream of it (unlike
# guard-destructive-generic.sh's cold-path toggles, which only read config
# lazily after a specific pattern has already matched). loom_resolve_config
# soft-reads all four tier files plus a merge (up to 5 jq forks) on every
# call; this direct single-jq read of the nearest .loom/config.json costs
# exactly one. Fanning that out to 5x on the single hottest guard invocation
# in the whole system is the #3687 fork-budget regression this repo has
# already decided against once (see guard-destructive-generic.sh's own
# CARVE-OUT for the read-only fast path). If Epic #3835's `.loom-project/`
# tier needs to reach this toggle, prefer caching a repo-scoped
# loom_resolve_config() result once per process (mirroring
# guard-destructive-generic.sh's per-invocation caches) rather than paying
# the full resolve on every Edit/Write.
# =============================================================================
worktree_isolation_guard_enabled() {
    local enabled=true
    if [[ -n "$MAIN_ROOT" && -f "$MAIN_ROOT/.loom/config.json" ]] && command -v jq &>/dev/null; then
        # jq // is alternative-on-null, not default-on-missing, so use
        # if/then/else to treat only an explicit `false` as disabled (a
        # missing guards.worktreeIsolation key stays on).
        enabled=$(jq -r 'if .guards.worktreeIsolation == false then "false" else "true" end' "$MAIN_ROOT/.loom/config.json" 2>/dev/null) || enabled=true
        [[ -n "$enabled" ]] || enabled=true
    fi
    case "${LOOM_GUARD_WORKTREE_ISOLATION:-}" in
        0|false|no)  enabled=false ;;
        1|true|yes)  enabled=true ;;
    esac
    [[ "$enabled" == "true" ]]
}

if ! worktree_isolation_guard_enabled; then
    exit 0
fi

# --------------------------------------------------------------------------
# Helpers for the path-derived fallback (mechanism 2 above)
# --------------------------------------------------------------------------

# Walk up from $1 (a path that may or may not exist) looking for a
# `.loom-managed` sentinel. Prints the sentinel's directory and returns 0 if
# found; returns 1 if none is found by the time we reach filesystem root.
# Pure string manipulation (no subprocess forks, no `cd`) so it never errors
# on a path whose parent directories don't exist yet (new file via Write).
walk_up_for_sentinel() {
    local dir="$1"
    if [[ ! -d "$dir" ]]; then
        dir="${dir%/*}"
        [[ -z "$dir" ]] && dir="/"
    fi
    local i=0
    while [[ $i -lt 64 ]]; do
        if [[ -f "$dir/.loom-managed" ]]; then
            printf '%s' "$dir"
            return 0
        fi
        [[ "$dir" == "/" ]] && break
        dir="${dir%/*}"
        [[ -z "$dir" ]] && dir="/"
        i=$((i + 1))
    done
    return 1
}

# Resolve the base directory that holds managed worktrees, honoring the same
# override precedence as `defaults/scripts/lib/worktree-root.sh`
# (LOOM_WORKTREE_ROOT env > .loom/config.json worktree.root > default). Kept
# as a small inline duplicate rather than sourcing the library, so this
# guard's fail-open contract does not depend on another script's behavior.
#
# CARVE-OUT (#4241, same class as #4063): the `worktree.root` read below is
# deliberately NOT routed through config-resolver.sh either, for a different
# reason than the toggle above -- this function is only reached on the rare
# deny-candidate path (target resolves inside the main checkout and outside
# every worktree, see path_derived_allow() above), so fork cost is not the
# concern here. The concern is the SAME self-containment rationale already
# documented on this function: sourcing config-resolver.sh would make this
# guard's fail-open contract depend on that library's own behavior (and
# transitively on jq's `*` deep-merge across up to four tier files) instead
# of one direct, independently-auditable jq read. If/when a repo actually
# populates `.loom-project/project.json` -> worktree.root (Epic #3835 Phase
# 6+), migrate this read (and worktree-root.sh's own resolution) together so
# the two stay in lockstep -- not this hook alone.
resolve_worktree_base() {
    if [[ -n "${LOOM_WORKTREE_ROOT:-}" && "${LOOM_WORKTREE_ROOT}" == /* ]]; then
        printf '%s' "${LOOM_WORKTREE_ROOT%/}/$(basename "$MAIN_ROOT")"
        return 0
    fi
    if [[ -n "$MAIN_ROOT" && -f "$MAIN_ROOT/.loom/config.json" ]] && command -v jq &>/dev/null; then
        local cfg_root
        cfg_root=$(jq -r '.worktree.root? // empty' "$MAIN_ROOT/.loom/config.json" 2>/dev/null) || cfg_root=""
        if [[ -n "$cfg_root" && "$cfg_root" == /* ]]; then
            printf '%s' "${cfg_root%/}/$(basename "$MAIN_ROOT")"
            return 0
        fi
    fi
    printf '%s' "${MAIN_ROOT}/.loom/worktrees"
}

# True if at least one managed worktree currently exists under $1
# (`<base>/<name>/.loom-managed`, depth 2 -- matches worktree.sh's layout).
any_managed_worktree_exists() {
    local base="$1"
    [[ -n "$base" && -d "$base" ]] || return 1
    local hit
    hit=$(find "$base" -mindepth 2 -maxdepth 2 -name '.loom-managed' -print -quit 2>/dev/null) || hit=""
    [[ -n "$hit" ]]
}

# Decide allow(0)/deny(1) for a resolved target path when no
# LOOM_WORKTREE_PATH fast path is set. See the mechanism-2 comment above for
# the rationale.
path_derived_allow() {
    local target="$1"

    # (a) Already inside some managed worktree -> allow.
    if walk_up_for_sentinel "$target" >/dev/null; then
        return 0
    fi

    # Not under any worktree. If it's also not under the main checkout,
    # there's nothing this guard protects (e.g. /tmp scratch files) -> allow.
    local target_slash="${target%/}/" main_slash="${MAIN_ROOT%/}/"
    if [[ -z "$MAIN_ROOT" || ( "$target_slash" != "$main_slash"* && "$target" != "$MAIN_ROOT" ) ]]; then
        return 0
    fi

    # Target resolves inside the main checkout and outside every worktree.
    # Deny only if worktree isolation is actually in play for this
    # repo/session (a managed worktree exists somewhere); otherwise fail
    # open (a repo/session that has never created a worktree is unaffected).
    local base
    base="$(resolve_worktree_base)"
    if any_managed_worktree_exists "$base"; then
        return 1
    fi
    return 0
}

# Emit a deny decision and exit 0 (the hook itself never exits non-zero).
emit_deny() {
    local reason="$1"
    log_hook_error "Denied: $reason"
    if jq -n --arg reason "$reason" '{
        hookSpecificOutput: {
            permissionDecision: "deny",
            permissionDecisionReason: $reason
        }
    }' 2>/dev/null; then
        exit 0
    fi
    # jq failed — emit raw JSON as fallback
    local escaped
    escaped=$(echo "$reason" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\t/\\t/g')
    echo "{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"${escaped}\"}}"
    exit 0
}

# --------------------------------------------------------------------------
# Fast path: LOOM_WORKTREE_PATH (tmux/manual sessions)
# --------------------------------------------------------------------------
WORKTREE_PATH="${LOOM_WORKTREE_PATH:-}"

# Read stdin (needed by both mechanisms below)
INPUT=$(cat 2>/dev/null) || INPUT=""

# Verify jq is available
if ! command -v jq &>/dev/null; then
    log_hook_error "jq not found in PATH — allowing (cannot parse input)"
    exit 0
fi

# Extract file_path from tool input
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null) || FILE_PATH=""

if [[ -z "$FILE_PATH" ]]; then
    # No file_path in input (shouldn't happen for Edit/Write) — allow
    exit 0
fi

# Resolve the file path to absolute (handle relative paths via cwd)
if [[ "$FILE_PATH" != /* ]]; then
    CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null) || CWD=""
    if [[ -n "$CWD" ]]; then
        FILE_PATH="${CWD}/${FILE_PATH}"
    fi
fi

# Normalize the path: resolve .. and . components without requiring the file to exist.
# Use Python's os.path.normpath for reliable path normalization (handles ../ etc.)
# Falls back to the raw path if Python is unavailable.
NORM_PATH=$(printf '%s' "$FILE_PATH" | python3 -c "import os,sys; print(os.path.normpath(sys.stdin.read()))" 2>/dev/null) || NORM_PATH="$FILE_PATH"

if [[ -n "$WORKTREE_PATH" ]]; then
    # Normalize worktree path (resolve symlinks, remove trailing slash)
    WORKTREE_REAL=$(cd "$WORKTREE_PATH" 2>/dev/null && pwd -P 2>/dev/null) || WORKTREE_REAL="$WORKTREE_PATH"
    WORKTREE_REAL="${WORKTREE_REAL%/}"

    # Check if the normalized path starts with the worktree path
    if [[ "$NORM_PATH/" == "$WORKTREE_REAL/"* ]] || [[ "$NORM_PATH/" == "$WORKTREE_PATH/"* ]]; then
        # Path is within the worktree — allow
        exit 0
    fi

    emit_deny "BLOCKED: Edit/Write path '${NORM_PATH}' is outside worktree '${WORKTREE_PATH}'. Use paths within the worktree directory."
fi

# --------------------------------------------------------------------------
# Path-derived fallback: no LOOM_WORKTREE_PATH (daemon-dispatched sweep path)
# --------------------------------------------------------------------------
if path_derived_allow "$NORM_PATH"; then
    exit 0
fi

emit_deny "BLOCKED: Edit/Write path '${NORM_PATH}' resolves to the main repository checkout ('${MAIN_ROOT}'), but a Loom-managed worktree exists for this session. Builders must write inside their issue worktree (.loom/worktrees/issue-<N>), never the main checkout. Do NOT retry this write via Bash redirection/tee/sed -i/cp/mv -- that is also confined (guard-destructive-generic.sh, #4178) and denied for the same reason. cd into your issue worktree and write there instead. (#4007)"
