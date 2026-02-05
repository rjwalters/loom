#!/usr/bin/env bash
# guard-destructive.sh - PreToolUse hook to block destructive agent commands
#
# Claude Code PreToolUse hook that intercepts Bash commands before execution.
# Receives JSON on stdin with tool_input.command and cwd fields.
#
# Decisions:
#   - Block (deny): Dangerous commands that should never run
#   - Ask: Commands that need human confirmation
#   - Allow: Everything else (exit 0, no output)
#
# Output format (Claude Code hooks spec):
#   { "hookSpecificOutput": { "permissionDecision": "deny|ask", "permissionDecisionReason": "..." } }

set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')
CWD=$(echo "$INPUT" | jq -r '.cwd // empty')

# If no command to check, allow
if [[ -z "$COMMAND" ]]; then
    exit 0
fi

# Resolve repo root from cwd
REPO_ROOT=""
if [[ -n "$CWD" ]]; then
    REPO_ROOT=$(git -C "$CWD" rev-parse --show-toplevel 2>/dev/null || true)
fi

# Helper: output a deny decision and exit
deny() {
    local reason="$1"
    jq -n --arg reason "$reason" '{
        hookSpecificOutput: {
            permissionDecision: "deny",
            permissionDecisionReason: $reason
        }
    }'
    exit 0
}

# Helper: output an ask decision and exit
ask() {
    local reason="$1"
    jq -n --arg reason "$reason" '{
        hookSpecificOutput: {
            permissionDecision: "ask",
            permissionDecisionReason: $reason
        }
    }'
    exit 0
}

# =============================================================================
# ALWAYS BLOCK - Catastrophic commands that should never execute
# =============================================================================

ALWAYS_BLOCK_PATTERNS=(
    # GitHub destructive operations
    'gh repo delete'
    'gh repo archive'

    # Force push to main/master (various flag forms)
    'git push --force origin main'
    'git push --force origin master'
    'git push -f origin main'
    'git push -f origin master'
    'git push --force-with-lease origin main'
    'git push --force-with-lease origin master'

    # Filesystem destruction
    'rm -rf /'
    'rm -rf /\*'
    'rm -rf ~'
    'rm -rf \$HOME'

    # Fork bombs
    ':\(\)\{ :\|:& \};:'

    # Pipe to shell (supply chain risk)
    'curl .* \| .*sh'
    'curl .* \| bash'
    'wget .* \| .*sh'
    'wget .* -O- \| sh'

    # Cloud infrastructure destruction
    'aws s3 rm.*--recursive'
    'aws s3 rb'
    'aws ec2 terminate'
    'aws iam delete'
    'aws cloudformation delete-stack'
    'gcloud.*delete'
    'az.*delete'
    'az group delete'

    # Docker mass destruction
    'docker system prune'

    # Database destruction
    'DROP DATABASE'
    'DROP TABLE'
    'DROP SCHEMA'
    'TRUNCATE TABLE'
)

for pattern in "${ALWAYS_BLOCK_PATTERNS[@]}"; do
    if echo "$COMMAND" | grep -qiE "$pattern"; then
        deny "BLOCKED: Command matches dangerous pattern: $pattern"
    fi
done

# =============================================================================
# rm -rf SCOPE CHECK - Block rm with recursive/force flags outside repo
# =============================================================================

# Match rm commands with -r or -f flags (in any combination: -rf, -r -f, -fr, etc.)
if echo "$COMMAND" | grep -qE 'rm\s+(-[a-zA-Z]*[rf][a-zA-Z]*\s+)+' || \
   echo "$COMMAND" | grep -qE 'rm\s+-[a-zA-Z]*r[a-zA-Z]*\s' || \
   echo "$COMMAND" | grep -qE 'rm\s+-[a-zA-Z]*f[a-zA-Z]*\s+-[a-zA-Z]*r[a-zA-Z]*\s'; then

    # Extract target paths from the rm command (skip flags)
    TARGETS=$(echo "$COMMAND" | sed 's/rm\s\+//' | tr ' ' '\n' | grep -v '^-' | head -20)

    for target in $TARGETS; do
        # Skip empty targets
        [[ -z "$target" ]] && continue

        # Skip known-safe patterns (allowlist)
        case "$target" in
            node_modules|./node_modules|*/node_modules)
                continue ;;
            target|./target|*/target)
                continue ;;
            dist|./dist|*/dist)
                continue ;;
            build|./build|*/build)
                continue ;;
            .loom/worktrees/*|*/.loom/worktrees/*)
                continue ;;
            .next|./.next|*/.next)
                continue ;;
            __pycache__|./__pycache__|*/__pycache__)
                continue ;;
            .pytest_cache|./.pytest_cache|*/.pytest_cache)
                continue ;;
            *.pyc)
                continue ;;
        esac

        # Resolve path to absolute
        ABS_PATH=""
        if [[ "$target" = /* ]]; then
            ABS_PATH="$target"
        elif [[ -n "$CWD" ]]; then
            ABS_PATH=$(cd "$CWD" 2>/dev/null && realpath -m "$target" 2>/dev/null || echo "$CWD/$target")
        fi

        # Block dangerous absolute paths
        if [[ "$ABS_PATH" == "/" ]] || [[ "$ABS_PATH" == "/home" ]] || \
           [[ "$ABS_PATH" == "$HOME" ]] || [[ "$ABS_PATH" == "/tmp" ]] || \
           [[ "$ABS_PATH" == "/usr" ]] || [[ "$ABS_PATH" == "/var" ]] || \
           [[ "$ABS_PATH" == "/etc" ]] || [[ "$ABS_PATH" == "/opt" ]]; then
            deny "BLOCKED: rm on protected system path: $ABS_PATH"
        fi

        # Block if outside repo root (when we know the repo root)
        if [[ -n "$REPO_ROOT" ]] && [[ -n "$ABS_PATH" ]]; then
            if [[ "$ABS_PATH" != "$REPO_ROOT"* ]]; then
                deny "BLOCKED: rm target outside repository: $ABS_PATH (repo: $REPO_ROOT)"
            fi
        fi
    done
fi

# =============================================================================
# DELETE without WHERE - Database safety
# =============================================================================

if echo "$COMMAND" | grep -qiE 'DELETE\s+FROM\s+' && \
   ! echo "$COMMAND" | grep -qiE 'WHERE\s+'; then
    deny "BLOCKED: DELETE FROM without WHERE clause"
fi

# =============================================================================
# REQUIRE CONFIRMATION - Potentially dangerous but sometimes legitimate
# =============================================================================

ASK_PATTERNS=(
    # Git destructive operations (not on main/master - those are blocked above)
    'git push --force'
    'git push -f '
    'git reset --hard'
    'git clean -fd'
    'git checkout \.'
    'git restore \.'

    # GitHub operations that modify shared state
    'gh pr close'
    'gh issue close'
    'gh release delete'
    'gh label delete'

    # Cloud CLI operations
    'aws s3'
    'aws ec2'
    'aws lambda'

    # Docker operations
    'docker rm'
    'docker rmi'
    'docker stop'
    'docker kill'

    # Credential exposure
    'printenv.*SECRET'
    'printenv.*TOKEN'
    'printenv.*KEY'
    'cat.*/\.ssh/'
    'cat.*/\.aws/credentials'
)

for pattern in "${ASK_PATTERNS[@]}"; do
    if echo "$COMMAND" | grep -qE "$pattern"; then
        ask "Command requires confirmation: $COMMAND"
    fi
done

# =============================================================================
# ALLOW - Everything else passes through
# =============================================================================

exit 0
