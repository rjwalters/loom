#!/bin/bash
# claude-wrapper.sh - Resilient Claude CLI wrapper with retry logic
#
# This wrapper script handles transient API failures from the Claude CLI
# by implementing exponential backoff retry logic. It's designed for use
# with autonomous agents in Loom terminals.
#
# Features:
# - Pre-flight checks (CLI availability, API reachability)
# - Error pattern detection for known transient failures
# - Exponential backoff with configurable parameters
# - Graceful shutdown via stop signal file
# - Working directory recovery (handles deleted worktrees)
# - Detailed logging for debugging
#
# Usage:
#   ./claude-wrapper.sh [claude arguments]
#   ./claude-wrapper.sh --dangerously-skip-permissions
#
# Environment Variables:
#   LOOM_MAX_RETRIES       - Maximum retry attempts (default: 5)
#   LOOM_INITIAL_WAIT      - Initial wait time in seconds (default: 60)
#   LOOM_MAX_WAIT          - Maximum wait time in seconds (default: 1800 = 30min)
#   LOOM_BACKOFF_MULTIPLIER - Backoff multiplier (default: 2)
#   LOOM_TERMINAL_ID       - Terminal ID for stop signal (optional)
#   LOOM_WORKSPACE         - Workspace path for stop signal (optional)
#   LOOM_AUTH_CACHE_TTL    - Auth cache TTL in seconds (default: 120)
#   LOOM_AUTH_CACHE_STALE_LOCK_THRESHOLD - Stale lock cleanup threshold in seconds (default: 90)
#   LOOM_AUTH_CACHE_LOCK_WAIT - Max time to wait for lock holder in seconds (default: 60)

set -euo pipefail

# Configuration with environment variable overrides
MAX_RETRIES="${LOOM_MAX_RETRIES:-5}"
INITIAL_WAIT="${LOOM_INITIAL_WAIT:-60}"
MAX_WAIT="${LOOM_MAX_WAIT:-1800}"  # 30 minutes
MULTIPLIER="${LOOM_BACKOFF_MULTIPLIER:-2}"

# Output monitor configuration
# How long to wait after detecting an API error pattern before killing claude
API_ERROR_IDLE_TIMEOUT="${LOOM_API_ERROR_IDLE_TIMEOUT:-60}"

# Auth cache configuration
# Short-TTL file cache to prevent concurrent `claude auth status` calls
# from overwhelming the auth endpoint when multiple agents start simultaneously
AUTH_CACHE_TTL="${LOOM_AUTH_CACHE_TTL:-120}"  # seconds
# Max time a single auth check cycle can take: 15s timeout × 3 retries + backoff (2+5+10) ≈ 62s
AUTH_CACHE_STALE_LOCK_THRESHOLD="${LOOM_AUTH_CACHE_STALE_LOCK_THRESHOLD:-90}"  # seconds
AUTH_CACHE_LOCK_WAIT="${LOOM_AUTH_CACHE_LOCK_WAIT:-60}"  # seconds

# Startup health monitor configuration
# How long (seconds) to watch early output for MCP/plugin failures
STARTUP_MONITOR_WINDOW="${LOOM_STARTUP_MONITOR_WINDOW:-90}"
# Grace period (seconds) after detecting startup failure before killing
STARTUP_GRACE_PERIOD="${LOOM_STARTUP_GRACE_PERIOD:-10}"

# Terminal identification for stop signals
TERMINAL_ID="${LOOM_TERMINAL_ID:-}"
# Note: WORKSPACE may fail if CWD is invalid at startup - recover_cwd handles this
WORKSPACE="${LOOM_WORKSPACE:-$(pwd 2>/dev/null || echo "$HOME")}"

# Whether --dangerously-skip-permissions was passed (detected in main())
SKIP_PERMISSIONS_MODE=false

# Retry state file for external observability (see issue #2296).
# When TERMINAL_ID is set, the wrapper writes its retry/backoff state to this
# file so agent-wait-bg.sh and the shepherd can distinguish "wrapper retrying"
# from "claude actively working".
RETRY_STATE_DIR="${WORKSPACE}/.loom/retry-state"
RETRY_STATE_FILE=""
if [[ -n "${TERMINAL_ID}" ]]; then
    RETRY_STATE_FILE="${RETRY_STATE_DIR}/${TERMINAL_ID}.json"
fi

# Logging helpers
log_info() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] [INFO] $*" >&2
}

log_warn() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] [WARN] $*" >&2
}

log_error() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] [ERROR] $*" >&2
}

# Write retry state to a JSON file for external observability (issue #2296).
# Called when entering backoff or starting a new attempt so the shepherd
# and agent-wait-bg.sh can see what the wrapper is doing.
write_retry_state() {
    if [[ -z "${RETRY_STATE_FILE}" ]]; then
        return
    fi
    local status="$1"
    local attempt="$2"
    local last_error="${3:-}"
    local next_retry_at="${4:-}"

    mkdir -p "${RETRY_STATE_DIR}"
    cat > "${RETRY_STATE_FILE}" <<EOJSON
{
  "status": "${status}",
  "attempt": ${attempt},
  "max_retries": ${MAX_RETRIES},
  "last_error": $(printf '%s' "${last_error}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || echo '""'),
  "next_retry_at": "${next_retry_at}",
  "terminal_id": "${TERMINAL_ID}",
  "updated_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON
}

# Remove the retry state file on exit (success or permanent failure).
clear_retry_state() {
    if [[ -n "${RETRY_STATE_FILE}" ]] && [[ -f "${RETRY_STATE_FILE}" ]]; then
        rm -f "${RETRY_STATE_FILE}"
    fi
}

# Recover from deleted working directory
# This handles the case where the agent's worktree is deleted while it's running
# (e.g., by loom-clean, merge-pr.sh, or agent-destroy.sh)
recover_cwd() {
    # Check if current directory is still valid
    if pwd &>/dev/null 2>&1; then
        return 0  # CWD is fine, nothing to do
    fi

    log_warn "Working directory deleted, attempting recovery..."

    # Try WORKSPACE first (set by agent-spawn.sh, may point to repo root)
    if [[ -n "${WORKSPACE:-}" ]] && [[ -d "$WORKSPACE" ]]; then
        if cd "$WORKSPACE" 2>/dev/null; then
            log_info "Recovered to workspace: $WORKSPACE"
            return 0
        fi
    fi

    # Try to find git root (may fail if CWD context is completely gone)
    local git_root
    if git_root=$(git rev-parse --show-toplevel 2>/dev/null) && [[ -d "$git_root" ]]; then
        if cd "$git_root" 2>/dev/null; then
            log_info "Recovered to git root: $git_root"
            return 0
        fi
    fi

    # Last resort: home directory
    if cd "$HOME" 2>/dev/null; then
        log_warn "Recovered to HOME (worktree likely removed): $HOME"
        return 0
    fi

    # Absolute last resort: /tmp
    if cd /tmp 2>/dev/null; then
        log_warn "Recovered to /tmp (all other recovery paths failed)"
        return 0
    fi

    log_error "Failed to recover working directory - all recovery paths failed"
    return 1
}

# Check if stop signal exists (graceful shutdown support)
check_stop_signal() {
    # Global stop signal for all agents
    if [[ -f "${WORKSPACE}/.loom/stop-daemon" ]]; then
        log_info "Global stop signal detected (.loom/stop-daemon)"
        return 0
    fi

    # Per-terminal stop signal
    if [[ -n "${TERMINAL_ID}" && -f "${WORKSPACE}/.loom/stop-agent-${TERMINAL_ID}" ]]; then
        log_info "Agent stop signal detected (.loom/stop-agent-${TERMINAL_ID})"
        return 0
    fi

    return 1
}

# Resolve workspace root for MCP config lookup.
# In worktrees, WORKSPACE may point to the worktree itself; the MCP config
# (.mcp.json) lives in the git common directory (the main checkout).
resolve_mcp_workspace() {
    # If .mcp.json exists in WORKSPACE, use it directly
    if [[ -f "${WORKSPACE}/.mcp.json" ]]; then
        echo "${WORKSPACE}"
        return
    fi

    # In a worktree, try the git common directory (main checkout)
    local common_dir
    if common_dir=$(git -C "${WORKSPACE}" rev-parse --git-common-dir 2>/dev/null); then
        # common_dir is the .git dir; parent is the repo root
        local repo_root
        repo_root=$(cd "${common_dir}/.." 2>/dev/null && pwd)
        if [[ -f "${repo_root}/.mcp.json" ]]; then
            echo "${repo_root}"
            return
        fi
    fi

    # Fallback to WORKSPACE
    echo "${WORKSPACE}"
}

# Pre-flight check: verify MCP server can start
# Attempts to launch the mcp-loom Node.js server and checks for the startup
# message on stderr. If the dist/ directory is missing or stale, attempts
# a rebuild before retrying.
check_mcp_server() {
    local mcp_workspace
    mcp_workspace=$(resolve_mcp_workspace)

    local mcp_config="${mcp_workspace}/.mcp.json"
    if [[ ! -f "${mcp_config}" ]]; then
        log_warn "MCP config not found at ${mcp_config} - skipping MCP pre-flight"
        return 0  # Non-fatal: MCP may not be configured
    fi

    # Extract the MCP server entry point from .mcp.json
    # Use timeout to prevent hanging on resource-contended systems (see issue #2472).
    local mcp_entry
    mcp_entry=$(timeout 10 python3 -c "
import json, sys
with open('${mcp_config}') as f:
    cfg = json.load(f)
servers = cfg.get('mcpServers', {})
for name, srv in servers.items():
    args = srv.get('args', [])
    if args:
        print(args[-1])
        sys.exit(0)
" 2>/dev/null || echo "")

    if [[ -z "${mcp_entry}" ]]; then
        log_warn "Could not extract MCP entry point from ${mcp_config} - skipping MCP pre-flight"
        return 0
    fi

    # Check if the entry point file exists
    if [[ ! -f "${mcp_entry}" ]]; then
        log_warn "MCP entry point missing: ${mcp_entry}"
        _try_mcp_rebuild "${mcp_entry}"
        return $?
    fi

    # Smoke test: start MCP server and verify it emits the startup message
    # The MCP server writes "Loom MCP server running on stdio" to stderr on success.
    # Use a short timeout - we just need to see the startup message.
    local mcp_stderr
    mcp_stderr=$(timeout 5 node "${mcp_entry}" </dev/null 2>&1 || true)

    if echo "${mcp_stderr}" | grep -qi "running on stdio"; then
        log_info "MCP server health check passed"
        return 0
    fi

    # MCP server failed to start - log the error
    log_warn "MCP server health check failed"
    if [[ -n "${mcp_stderr}" ]]; then
        log_warn "MCP stderr: ${mcp_stderr}"
    fi

    # Attempt rebuild and retry
    _try_mcp_rebuild "${mcp_entry}"
    return $?
}

# Attempt to rebuild the MCP server and re-verify
_try_mcp_rebuild() {
    local mcp_entry="$1"

    # Derive the package directory from the entry point
    # e.g., /path/to/mcp-loom/dist/index.js -> /path/to/mcp-loom
    local mcp_dir
    mcp_dir=$(dirname "$(dirname "${mcp_entry}")")

    if [[ ! -f "${mcp_dir}/package.json" ]]; then
        log_error "MCP package directory not found at ${mcp_dir} - cannot rebuild"
        return 1
    fi

    log_info "Attempting MCP server rebuild in ${mcp_dir}..."

    # Run npm build (suppressing verbose output)
    if (cd "${mcp_dir}" && npm run build 2>&1 | tail -5) >&2; then
        log_info "MCP rebuild completed"
    else
        log_error "MCP rebuild failed"
        return 1
    fi

    # Re-check after rebuild
    if [[ ! -f "${mcp_entry}" ]]; then
        log_error "MCP entry point still missing after rebuild: ${mcp_entry}"
        return 1
    fi

    local mcp_stderr
    mcp_stderr=$(timeout 5 node "${mcp_entry}" </dev/null 2>&1 || true)

    if echo "${mcp_stderr}" | grep -qi "running on stdio"; then
        log_info "MCP server health check passed after rebuild"
        return 0
    fi

    log_error "MCP server still fails after rebuild"
    if [[ -n "${mcp_stderr}" ]]; then
        log_error "MCP stderr after rebuild: ${mcp_stderr}"
    fi
    return 1
}

# Pre-flight check: verify Claude CLI is available
check_cli_available() {
    if ! command -v claude &>/dev/null; then
        log_error "Claude CLI not found in PATH"
        log_error "Install with: npm install -g @anthropic-ai/claude-code"
        return 1
    fi
    log_info "Claude CLI found: $(command -v claude)"
    return 0
}

# --- Auth cache helpers ---
# Prevent concurrent `claude auth status` calls from overwhelming the auth
# endpoint when multiple agents start simultaneously (thundering-herd protection).
# Cache is user-scoped and short-lived; any failure falls through to a direct call.

# Return a random integer in [1, max] using $RANDOM (portable bash).
# Used to add jitter and desynchronize concurrent agents.
_auth_jitter() {
    local max="${1:-5}"
    echo $(( (RANDOM % max) + 1 ))
}

_auth_cache_file() {
    echo "/tmp/claude-auth-cache-$(id -u).json"
}

_auth_lock_dir() {
    echo "/tmp/claude-auth-cache-$(id -u).lock"
}

# Acquire the cache lock (non-blocking).
# Returns 0 if acquired, 1 if another process holds it.
# Cleans up stale locks older than AUTH_CACHE_STALE_LOCK_THRESHOLD first.
_auth_cache_lock() {
    local lock_dir
    lock_dir=$(_auth_lock_dir)

    # Clean up stale locks (process that created it likely died)
    if [[ -d "${lock_dir}" ]]; then
        local lock_age=0
        if [[ "$(uname)" == "Darwin" ]]; then
            lock_age=$(( $(date +%s) - $(stat -f '%m' "${lock_dir}" 2>/dev/null || echo "0") ))
        else
            lock_age=$(( $(date +%s) - $(stat -c '%Y' "${lock_dir}" 2>/dev/null || echo "0") ))
        fi
        if [[ "${lock_age}" -gt "${AUTH_CACHE_STALE_LOCK_THRESHOLD}" ]]; then
            log_info "Removing stale auth cache lock (age: ${lock_age}s, threshold: ${AUTH_CACHE_STALE_LOCK_THRESHOLD}s)"
            rmdir "${lock_dir}" 2>/dev/null || true
        fi
    fi

    # Atomic lock acquisition via mkdir
    mkdir "${lock_dir}" 2>/dev/null
}

# Release the cache lock.
_auth_cache_unlock() {
    local lock_dir
    lock_dir=$(_auth_lock_dir)
    rmdir "${lock_dir}" 2>/dev/null || true
}

# Read cached auth output if it exists and is within TTL.
# On success, echoes the cached auth JSON output and returns 0.
# Returns 1 if cache is missing, corrupt, or expired.
_auth_cache_read() {
    local cache_file
    cache_file=$(_auth_cache_file)

    if [[ ! -f "${cache_file}" ]]; then
        return 1
    fi

    # Parse cache: extract time, exit_code, and output
    local cache_time cache_exit cached_output
    cache_time=$(python3 -c "import json,sys; d=json.load(open('${cache_file}')); print(d['time'])" 2>/dev/null) || return 1
    cache_exit=$(python3 -c "import json,sys; d=json.load(open('${cache_file}')); print(d['exit_code'])" 2>/dev/null) || return 1

    # Check TTL
    local now
    now=$(date +%s)
    local age=$(( now - cache_time ))
    if [[ "${age}" -gt "${AUTH_CACHE_TTL}" ]]; then
        return 1
    fi

    # Only use cache if the original call succeeded
    if [[ "${cache_exit}" -ne 0 ]]; then
        return 1
    fi

    # Extract the output field
    cached_output=$(python3 -c "import json,sys; print(json.load(open('${cache_file}'))['output'])" 2>/dev/null) || return 1
    echo "${cached_output}"
    return 0
}

# Write auth output to the cache file.
# Arguments: $1 = auth JSON output, $2 = exit code
_auth_cache_write() {
    local output="$1"
    local exit_code="$2"
    local cache_file
    cache_file=$(_auth_cache_file)
    local now
    now=$(date +%s)

    python3 -c "
import json, sys
data = {
    'time': ${now},
    'exit_code': ${exit_code},
    'output': sys.stdin.read()
}
with open('${cache_file}', 'w') as f:
    json.dump(data, f)
" <<< "${output}" 2>/dev/null || true
}

# Pre-flight check: verify authentication status
# Uses `claude auth status --json` to confirm the CLI is logged in.
# When CLAUDE_CONFIG_DIR is set, passes it through so the check uses
# the same config the session will use.
check_auth_status() {
    local auth_output
    local auth_exit_code

    # --- Step 1: Try cache first ---
    local cached_output
    if cached_output=$(_auth_cache_read); then
        local logged_in
        logged_in=$(echo "${cached_output}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('loggedIn', False))" 2>/dev/null || echo "")
        if [[ "${logged_in}" == "True" ]]; then
            log_info "Authentication check passed (cached)"
            return 0
        fi
        # Cache says not logged in — fall through to fresh check
    fi

    # --- Step 2: Try to acquire lock for fresh check ---
    local lock_acquired=false
    if _auth_cache_lock; then
        lock_acquired=true
    else
        # Another process is refreshing — wait for it to finish, polling cache
        log_info "Auth cache lock held by another process, waiting up to ${AUTH_CACHE_LOCK_WAIT}s..."
        local wait_elapsed=0
        while [[ "${wait_elapsed}" -lt "${AUTH_CACHE_LOCK_WAIT}" ]]; do
            sleep 2
            wait_elapsed=$((wait_elapsed + 2))
            if cached_output=$(_auth_cache_read); then
                local logged_in
                logged_in=$(echo "${cached_output}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('loggedIn', False))" 2>/dev/null || echo "")
                if [[ "${logged_in}" == "True" ]]; then
                    log_info "Authentication check passed (cached, after ${wait_elapsed}s wait)"
                    return 0
                fi
            fi
        done
        # Still stale — fall through to direct check with jitter to desynchronize
        local jitter
        jitter=$(_auth_jitter 5)
        log_info "Auth cache still stale after ${AUTH_CACHE_LOCK_WAIT}s wait, proceeding with direct check (jitter: ${jitter}s)"
        sleep "${jitter}"
    fi

    # --- Step 3: Existing retry logic (with cache write on success) ---
    local max_retries=3
    local -a backoff_seconds=(2 5 10)

    for (( attempt=1; attempt<=max_retries; attempt++ )); do
        auth_exit_code=0

        # Refresh lock mtime so other processes don't consider it stale
        # while we're still actively retrying
        if [[ "${lock_acquired}" == "true" ]]; then
            touch "$(_auth_lock_dir)" 2>/dev/null || true
        fi

        # Unset CLAUDECODE to avoid nested-session guard when running inside
        # a Claude Code session (e.g., during testing or shepherd-spawned builds).
        # Use timeout to prevent hanging after a long first attempt leaves
        # auth in a bad state (see issue #2472).
        auth_output=$(timeout 15 bash -c 'CLAUDECODE="" claude auth status --json 2>&1') || auth_exit_code=$?

        # timeout exits with 124 when the command times out
        if [[ "${auth_exit_code}" -eq 124 ]]; then
            if (( attempt < max_retries )); then
                local backoff=${backoff_seconds[$((attempt - 1))]}
                local jitter
                jitter=$(_auth_jitter 3)
                backoff=$((backoff + jitter))
                log_info "Authentication check timed out (attempt ${attempt}/${max_retries}), retrying in ${backoff}s (includes ${jitter}s jitter)..."
                sleep "${backoff}"
                continue
            fi
            log_warn "Authentication check timed out after ${max_retries} attempts"
            [[ "${lock_acquired}" == "true" ]] && _auth_cache_unlock
            return 1
        fi

        if [[ "${auth_exit_code}" -ne 0 ]]; then
            log_warn "Authentication check command failed (exit ${auth_exit_code})"
            log_warn "Output: ${auth_output}"
            if [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
                log_warn "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR}"
                log_warn "Run: CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR} claude auth login"
            else
                log_warn "Run: claude auth login"
            fi
            [[ "${lock_acquired}" == "true" ]] && _auth_cache_unlock
            return 1
        fi

        # Write successful result to cache (best-effort)
        _auth_cache_write "${auth_output}" "${auth_exit_code}"

        # Parse the loggedIn field from JSON output
        local logged_in
        logged_in=$(echo "${auth_output}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('loggedIn', False))" 2>/dev/null || echo "")

        if [[ "${logged_in}" != "True" ]]; then
            log_warn "Authentication check failed: not logged in"
            if [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
                log_warn "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR}"
                log_warn "Run: CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR} claude auth login"
            else
                log_warn "Run: claude auth login"
            fi
            [[ "${lock_acquired}" == "true" ]] && _auth_cache_unlock
            return 1
        fi

        log_info "Authentication check passed (logged in)"
        [[ "${lock_acquired}" == "true" ]] && _auth_cache_unlock
        return 0
    done

    # Should not reach here, but guard against it
    log_warn "Authentication check failed after ${max_retries} attempts"
    [[ "${lock_acquired}" == "true" ]] && _auth_cache_unlock
    return 1
}

# Pre-flight check: verify API is reachable
# Uses a lightweight HEAD request to api.anthropic.com
check_api_reachable() {
    local timeout=10

    # Try curl first (most common)
    if command -v curl &>/dev/null; then
        if curl --silent --head --max-time "${timeout}" https://api.anthropic.com/ &>/dev/null; then
            log_info "API endpoint reachable (curl)"
            return 0
        fi
    fi

    # Fallback to nc (netcat)
    if command -v nc &>/dev/null; then
        if nc -z -w "${timeout}" api.anthropic.com 443 2>/dev/null; then
            log_info "API endpoint reachable (nc)"
            return 0
        fi
    fi

    log_warn "Could not verify API reachability (continuing anyway)"
    return 0  # Don't fail on network check - let Claude CLI handle it
}

# Detect if error output indicates a transient/retryable error
is_transient_error() {
    local output="$1"
    local exit_code="${2:-1}"

    # Known transient error patterns
    local patterns=(
        "No messages returned"
        "Rate limit exceeded"
        "rate_limit"
        "Connection refused"
        "ECONNREFUSED"
        "network error"
        "NetworkError"
        "ETIMEDOUT"
        "ECONNRESET"
        "ENETUNREACH"
        "socket hang up"
        "503 Service"
        "502 Bad Gateway"
        "500 Internal Server Error"
        "overloaded"
        "temporarily unavailable"
        "MCP server failed"
        "MCP.*failed"
        "plugins failed"
        "plugin.*failed to install"
    )

    for pattern in "${patterns[@]}"; do
        if echo "${output}" | grep -qi "${pattern}"; then
            log_info "Detected transient error pattern: ${pattern}"
            return 0
        fi
    done

    # Exit code 1 with no output often indicates API issues
    if [[ "${exit_code}" -eq 1 && -z "${output}" ]]; then
        log_info "Empty output with exit code 1 - treating as transient"
        return 0
    fi

    return 1
}

# Monitor output file for API errors during execution.
# If an API error pattern is detected and no new output arrives within
# API_ERROR_IDLE_TIMEOUT seconds, sends SIGINT to the claude process.
# This handles the "agent waits for 'try again' input" scenario.
#
# Arguments: $1 = output file path, $2 = PID file path to write monitor PID
start_output_monitor() {
    local output_file="$1"
    local monitor_pid_file="$2"

    (
        local last_size=0
        local error_detected_at=0

        while true; do
            sleep 5

            # Exit if output file is gone (session ended)
            if [[ ! -f "${output_file}" ]]; then
                break
            fi

            local current_size
            current_size=$(wc -c < "${output_file}" 2>/dev/null || echo "0")

            if [[ "${current_size}" -ne "${last_size}" ]]; then
                # New output arrived - check for API error patterns
                local tail_content
                tail_content=$(tail -c 2000 "${output_file}" 2>/dev/null || echo "")

                local found_error=false
                for pattern in "500 Internal Server Error" "Rate limit exceeded" \
                    "overloaded" "temporarily unavailable" "503 Service" \
                    "502 Bad Gateway" "No messages returned" \
                    "PreToolUse.*hook error"; do
                    if echo "${tail_content}" | grep -qi "${pattern}" 2>/dev/null; then
                        found_error=true
                        break
                    fi
                done

                if [[ "${found_error}" == "true" ]]; then
                    if [[ "${error_detected_at}" -eq 0 ]]; then
                        error_detected_at=$(date +%s)
                        log_warn "Output monitor: API error pattern detected, watching for idle..."
                    fi
                else
                    # New non-error output - reset detection
                    error_detected_at=0
                fi
                last_size="${current_size}"
            elif [[ "${error_detected_at}" -gt 0 ]]; then
                # No new output since error was detected
                local now
                now=$(date +%s)
                local idle_time=$((now - error_detected_at))
                if [[ "${idle_time}" -ge "${API_ERROR_IDLE_TIMEOUT}" ]]; then
                    log_warn "Output monitor: No new output for ${idle_time}s after API error - sending SIGINT to claude"
                    # Find and signal the claude process (child of this wrapper's shell)
                    pkill -INT -P $$ -f "claude" 2>/dev/null || true
                    break
                fi
            fi
        done
    ) &
    echo $! > "${monitor_pid_file}"
}

# Stop the background output monitor
stop_output_monitor() {
    local monitor_pid_file="$1"
    if [[ -f "${monitor_pid_file}" ]]; then
        local pid
        pid=$(cat "${monitor_pid_file}" 2>/dev/null || echo "")
        if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
            kill "${pid}" 2>/dev/null || true
            wait "${pid}" 2>/dev/null || true
        fi
        rm -f "${monitor_pid_file}"
    fi
}

# Monitor early CLI output for MCP/plugin startup failures.
# If the CLI starts with failed MCP servers or plugins, it often runs in a
# degraded state (stuck in thinking loops with no meaningful tool calls) rather
# than crashing.  This monitor watches the first STARTUP_MONITOR_WINDOW seconds
# of output for failure indicators and kills the CLI session so the retry loop
# can restart it cleanly.
#
# Arguments: $1 = output file path, $2 = PID file path to write monitor PID
start_startup_monitor() {
    local output_file="$1"
    local monitor_pid_file="$2"

    (
        local check_interval=5
        local elapsed=0

        while [[ "${elapsed}" -lt "${STARTUP_MONITOR_WINDOW}" ]]; do
            sleep "${check_interval}"
            elapsed=$((elapsed + check_interval))

            # Exit if output file is gone (session ended)
            if [[ ! -f "${output_file}" ]]; then
                break
            fi

            # Check first 50 lines for startup failure patterns
            local head_content
            head_content=$(head -50 "${output_file}" 2>/dev/null || echo "")

            if [[ -z "${head_content}" ]]; then
                continue
            fi

            local found_failure=false
            local matched_pattern=""
            for pattern in \
                "MCP server failed" \
                "MCP servers failed" \
                "plugins failed to install" \
                "plugins failed" \
                "plugin failed to install" \
                "plugin failed"; do
                if echo "${head_content}" | grep -qi "${pattern}" 2>/dev/null; then
                    found_failure=true
                    matched_pattern="${pattern}"
                    break
                fi
            done

            if [[ "${found_failure}" == "true" ]]; then
                log_warn "Startup monitor: detected '${matched_pattern}' in early output"
                log_warn "Startup monitor: waiting ${STARTUP_GRACE_PERIOD}s grace period before kill"
                sleep "${STARTUP_GRACE_PERIOD}"

                # Re-check: if output file is gone, session already ended
                if [[ ! -f "${output_file}" ]]; then
                    break
                fi

                log_warn "Startup monitor: killing degraded CLI session for retry"
                pkill -INT -P $$ -f "claude" 2>/dev/null || true
                break
            fi
        done
    ) &
    echo $! > "${monitor_pid_file}"
}

# Calculate wait time with exponential backoff
calculate_wait_time() {
    local attempt="$1"
    local wait_time=$((INITIAL_WAIT * (MULTIPLIER ** (attempt - 1))))

    # Cap at maximum wait time
    if [[ "${wait_time}" -gt "${MAX_WAIT}" ]]; then
        wait_time="${MAX_WAIT}"
    fi

    echo "${wait_time}"
}

# Format seconds as human-readable duration
format_duration() {
    local seconds="$1"
    local minutes=$((seconds / 60))
    local remaining=$((seconds % 60))

    if [[ "${minutes}" -gt 0 ]]; then
        echo "${minutes}m ${remaining}s"
    else
        echo "${seconds}s"
    fi
}

# Main retry loop with exponential backoff
run_with_retry() {
    local attempt=1
    local exit_code=0
    local output=""

    # Recover CWD if it was deleted before we started
    if ! recover_cwd; then
        log_error "Cannot proceed - working directory recovery failed"
        return 1
    fi

    log_info "Starting Claude CLI with resilient wrapper"
    log_info "Configuration: max_retries=${MAX_RETRIES}, initial_wait=${INITIAL_WAIT}s, max_wait=${MAX_WAIT}s, multiplier=${MULTIPLIER}x"

    while [[ "${attempt}" -le "${MAX_RETRIES}" ]]; do
        # Recover CWD if it was deleted during previous attempt or backoff
        if ! recover_cwd; then
            log_error "Cannot proceed - working directory recovery failed"
            return 1
        fi

        # Check for stop signal before each attempt
        if check_stop_signal; then
            log_info "Stop signal detected - exiting gracefully"
            return 0
        fi

        log_info "Attempt ${attempt}/${MAX_RETRIES}: Starting Claude CLI"
        write_retry_state "running" "${attempt}"

        # Run Claude CLI, capturing both stdout and stderr
        # We need to capture output while also displaying it in real-time
        # Use a temp file to capture output for error detection
        local temp_output
        temp_output=$(mktemp)

        # Start background output monitor to detect API errors during execution
        local monitor_pid_file
        monitor_pid_file=$(mktemp)

        # Start startup health monitor to detect MCP/plugin failures in early output
        local startup_monitor_pid_file
        startup_monitor_pid_file=$(mktemp)

        # Run claude with all arguments passed to wrapper
        # Three execution modes for Claude CLI:
        #
        # 1. Slash command prompt detected (e.g., "/judge 2434"):
        #    Use --print mode for reliable one-shot execution.  Interactive mode
        #    (script -q) can be blocked by onboarding/promotional dialogs that
        #    require user interaction before the prompt is processed. (Issue #2438)
        #
        # 2. No prompt, TTY available (autonomous agents):
        #    Use macOS `script` to preserve TTY so Claude CLI sees isatty(stdout) = true.
        #    A plain pipe (`| tee`) would replace stdout with a pipe fd, causing Claude
        #    to switch to non-interactive --print mode.
        #
        # 3. No prompt, no TTY (spawned from Claude Code's Bash tool):
        #    Run claude directly with tee for error detection.
        start_output_monitor "${temp_output}" "${monitor_pid_file}"
        start_startup_monitor "${temp_output}" "${startup_monitor_pid_file}"
        # Write sentinel marker so _is_low_output_session() in the shepherd can
        # distinguish wrapper pre-flight output from actual Claude CLI output.
        # The "# " prefix means it is also filtered as a header line.
        echo "# CLAUDE_CLI_START" >&2
        set +e  # Temporarily disable errexit to capture exit code
        unset CLAUDECODE  # Prevent nested session guard from blocking subprocess
        # Export per-agent config dir if set (for session isolation)
        if [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
            export CLAUDE_CONFIG_DIR
        fi
        if [[ -n "${TMPDIR:-}" ]]; then
            export TMPDIR
        fi
        # Detect slash command prompt in arguments (e.g., "/judge 2434").
        # On-demand workers spawned by the shepherd receive a slash command
        # as a positional prompt argument.  When present, we MUST use --print
        # mode instead of interactive mode (script -q) because interactive mode
        # can be blocked by onboarding dialogs or promotional banners that
        # require user interaction before the prompt is processed.  --print
        # explicitly skips all interactive dialogs.  See issue #2438.
        _has_slash_cmd=false
        for _arg in "$@"; do
            case "$_arg" in
                --*|-*) ;;  # Skip flags
                /*) _has_slash_cmd=true; break ;;
            esac
        done

        if [[ "$_has_slash_cmd" == "true" ]]; then
            # Slash command prompt detected - use --print for reliable execution
            log_info "Slash command detected in arguments, using --print mode"
            claude --print "$@" 2>&1 | tee "${temp_output}"
            exit_code=${PIPESTATUS[0]}
        elif [ -t 0 ]; then
            # No prompt, TTY available - use script to preserve interactive mode
            script -q "${temp_output}" claude "$@"
            exit_code=$?
        else
            # No TTY (socket/pipe) - run claude directly, tee output for error detection
            log_info "No TTY available, running claude directly (non-interactive mode)"
            claude "$@" 2>&1 | tee "${temp_output}"
            exit_code=${PIPESTATUS[0]}
        fi
        set -e
        stop_output_monitor "${monitor_pid_file}"
        stop_output_monitor "${startup_monitor_pid_file}"

        output=$(cat "${temp_output}")

        # In --print mode, pipe-pane may not flush before session exit.
        # Append captured output to the log file so log-based heuristics
        # (_is_instant_exit, _is_mcp_failure) have content to analyze
        # and post-mortem debugging is possible.  See issue #2550.
        if [[ "$_has_slash_cmd" == "true" && -n "${TERMINAL_ID}" ]]; then
            local _log_file="${WORKSPACE}/.loom/logs/loom-${TERMINAL_ID}.log"
            if [[ -f "$_log_file" && -s "${temp_output}" ]]; then
                cat "${temp_output}" >> "$_log_file"
            fi
        fi

        rm -f "${temp_output}"

        # Check exit code
        if [[ "${exit_code}" -eq 0 ]]; then
            log_info "Claude CLI completed successfully"
            clear_retry_state
            return 0
        fi

        log_warn "Claude CLI exited with code ${exit_code}"

        # Check if this is a transient error worth retrying
        if ! is_transient_error "${output}" "${exit_code}"; then
            log_error "Non-transient error detected - not retrying"
            log_error "Output: ${output}"
            clear_retry_state
            return "${exit_code}"
        fi

        # Check for stop signal before waiting
        if check_stop_signal; then
            log_info "Stop signal detected - exiting gracefully"
            return 0
        fi

        # Calculate backoff wait time
        local wait_time
        wait_time=$(calculate_wait_time "${attempt}")

        if [[ "${attempt}" -lt "${MAX_RETRIES}" ]]; then
            log_warn "Transient error detected. Waiting $(format_duration "${wait_time}") before retry..."

            # Truncate error output for the retry state file (first 200 chars)
            local error_snippet="${output:0:200}"
            local next_retry_ts
            next_retry_ts=$(date -u -v+"${wait_time}"S '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
                || date -u -d "+${wait_time} seconds" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
                || echo "")
            write_retry_state "backoff" "${attempt}" "${error_snippet}" "${next_retry_ts}"

            # Sleep with periodic stop signal checks
            local elapsed=0
            while [[ "${elapsed}" -lt "${wait_time}" ]]; do
                if check_stop_signal; then
                    log_info "Stop signal detected during backoff - exiting gracefully"
                    return 0
                fi
                sleep 5
                elapsed=$((elapsed + 5))
            done

            log_info "Backoff complete, retrying..."
        fi

        attempt=$((attempt + 1))
    done

    log_error "Max retries (${MAX_RETRIES}) exceeded"
    log_error "Last error: ${output}"
    clear_retry_state
    return 1
}

# Run pre-flight checks
run_preflight_checks() {
    log_info "Running pre-flight checks..."

    if ! check_cli_available; then
        return 1
    fi

    check_api_reachable  # Non-fatal, just logs

    if [[ -n "${LOOM_SHEPHERD_TASK_ID:-}" ]]; then
        log_info "Skipping auth pre-flight (shepherd subprocess, task=${LOOM_SHEPHERD_TASK_ID})"
    elif ! check_auth_status; then
        if [[ "${SKIP_PERMISSIONS_MODE}" == "true" ]]; then
            log_warn "Authentication pre-flight check failed (non-fatal in --dangerously-skip-permissions mode)"
        else
            log_error "Authentication pre-flight check failed"
            # Write sentinel so the shepherd can distinguish auth failures from
            # generic low-output sessions and avoid futile retries.  See issue #2508.
            echo "# AUTH_PREFLIGHT_FAILED" >&2
            return 1
        fi
    fi

    if ! check_mcp_server; then
        log_error "MCP server pre-flight check failed"
        return 1
    fi

    log_info "Pre-flight checks passed"
    return 0
}

# Main entry point
main() {
    # Ensure retry state file is cleaned up on exit (normal or abnormal)
    trap clear_retry_state EXIT

    log_info "Claude wrapper starting"
    log_info "Arguments: $*"
    log_info "Workspace: ${WORKSPACE}"
    [[ -n "${TERMINAL_ID}" ]] && log_info "Terminal ID: ${TERMINAL_ID}"

    # Detect --dangerously-skip-permissions flag (automated agent mode)
    for arg in "$@"; do
        if [[ "$arg" == "--dangerously-skip-permissions" ]]; then
            SKIP_PERMISSIONS_MODE=true
            break
        fi
    done

    # Run pre-flight checks
    if ! run_preflight_checks; then
        exit 1
    fi

    # Check for stop signal before starting
    if check_stop_signal; then
        log_info "Stop signal already present - exiting without starting"
        exit 0
    fi

    # Run Claude with retry logic
    run_with_retry "$@"
    exit_code=$?

    log_info "Claude wrapper exiting with code ${exit_code}"
    exit "${exit_code}"
}

# Run main with all script arguments
main "$@"
