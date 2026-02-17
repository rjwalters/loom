#!/usr/bin/env bash
# guard-worktree-paths.sh - PreToolUse hook to confine Edit/Write to worktree
#
# When LOOM_WORKTREE_PATH is set (agent running in a worktree), this hook
# blocks Edit and Write tool calls whose file_path resolves outside the
# worktree directory. This prevents builders from escaping their worktree
# and modifying files in the main repository (see issue #2441).
#
# No-op when LOOM_WORKTREE_PATH is not set (human users, non-worktree agents).
#
# Input (JSON on stdin):
#   { "tool_input": { "file_path": "/path/to/file", ... }, "cwd": "/cwd" }
#
# Output:
#   Exit 0 with no output = allow
#   Exit 0 with JSON { "hookSpecificOutput": { "permissionDecision": "deny", ... } } = block

# Determine main repo root via git-common-dir (works from worktrees and subdirectories)
MAIN_ROOT="$(cd "$(git rev-parse --git-common-dir 2>/dev/null)/.." 2>/dev/null && pwd)" || \
MAIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." 2>/dev/null && pwd 2>/dev/null || echo ".")"
HOOK_ERROR_LOG="${MAIN_ROOT}/.loom/logs/hook-errors.log"

log_hook_error() {
    mkdir -p "$(dirname "$HOOK_ERROR_LOG")" 2>/dev/null || true
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] [guard-worktree-paths] $1" >> "$HOOK_ERROR_LOG" 2>/dev/null || true
}

# Top-level error trap: on ANY unexpected error, allow to prevent infinite retry loops
trap 'log_hook_error "Unexpected error on line ${LINENO}: ${BASH_COMMAND:-unknown} (exit=$?)"; exit 0' ERR

# No worktree constraint — allow everything
WORKTREE_PATH="${LOOM_WORKTREE_PATH:-}"
if [[ -z "$WORKTREE_PATH" ]]; then
    exit 0
fi

# Normalize worktree path (resolve symlinks, remove trailing slash)
WORKTREE_REAL=$(cd "$WORKTREE_PATH" 2>/dev/null && pwd -P 2>/dev/null) || WORKTREE_REAL="$WORKTREE_PATH"
WORKTREE_REAL="${WORKTREE_REAL%/}"

# Read stdin
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

# Check if the normalized path starts with the worktree path
if [[ "$NORM_PATH/" == "$WORKTREE_REAL/"* ]] || [[ "$NORM_PATH/" == "$WORKTREE_PATH/"* ]]; then
    # Path is within the worktree — allow
    exit 0
fi

# Path is outside the worktree — deny
REASON="BLOCKED: Edit/Write path '${NORM_PATH}' is outside worktree '${WORKTREE_PATH}'. Use paths within the worktree directory."
log_hook_error "Denied: $REASON"

if jq -n --arg reason "$REASON" '{
    hookSpecificOutput: {
        permissionDecision: "deny",
        permissionDecisionReason: $reason
    }
}' 2>/dev/null; then
    exit 0
fi

# jq failed — emit raw JSON as fallback
ESCAPED_REASON=$(echo "$REASON" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\t/\\t/g')
echo "{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"${ESCAPED_REASON}\"}}"
exit 0
