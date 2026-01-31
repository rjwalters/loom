#!/bin/bash
# loom-tools.sh - Shared helpers for locating and invoking loom-tools commands
#
# This library provides consistent patterns for:
#   1. Locating loom-tools (source dir or installed)
#   2. Running loom-tools commands with proper fallbacks
#   3. Providing helpful error messages when loom-tools is missing
#
# Usage:
#   source "$(dirname "${BASH_SOURCE[0]}")/lib/loom-tools.sh"
#   run_loom_tool "agent-spawn" "agent_spawn" "$@"

# Find the repository root from the script location
_find_repo_root() {
    local dir="${1:-$(pwd)}"
    while [[ "$dir" != "/" ]]; do
        if [[ -d "$dir/.git" ]] || [[ -d "$dir/.loom" ]]; then
            echo "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    return 1
}

# Locate loom-tools directory
# Sets LOOM_TOOLS_DIR if found, returns 1 if not
# Priority:
#   1. Local loom-tools in repo (for Loom source repository)
#   2. Loom source path recorded during installation (for target repos)
find_loom_tools() {
    local script_dir="${1:-$(pwd)}"
    local repo_root

    repo_root="$(_find_repo_root "$script_dir")" || return 1

    # Check for local loom-tools (Loom source repo)
    if [[ -d "$repo_root/loom-tools/src/loom_tools" ]]; then
        LOOM_TOOLS_DIR="$repo_root/loom-tools"
        LOOM_TOOLS_SRC="$repo_root/loom-tools/src"
        return 0
    fi

    # Check for recorded loom source path (target repo)
    if [[ -f "$repo_root/.loom/loom-source-path" ]]; then
        local loom_source
        loom_source="$(cat "$repo_root/.loom/loom-source-path")"
        if [[ -d "$loom_source/loom-tools/src/loom_tools" ]]; then
            LOOM_TOOLS_DIR="$loom_source/loom-tools"
            LOOM_TOOLS_SRC="$loom_source/loom-tools/src"
            return 0
        fi
    fi

    LOOM_TOOLS_DIR=""
    LOOM_TOOLS_SRC=""
    return 1
}

# Run a loom-tools command with proper fallback chain
# Arguments:
#   $1 - CLI command name (e.g., "agent-spawn" for loom-agent-spawn)
#   $2 - Python module name (e.g., "agent_spawn" for loom_tools.agent_spawn)
#   $@ - Arguments to pass to the command
#
# Priority:
#   1. System-installed CLI (loom-<name>)
#   2. venv CLI in loom-tools directory
#   3. Python module with PYTHONPATH set
#   4. Error with helpful message
run_loom_tool() {
    local cli_name="$1"
    local module_name="$2"
    shift 2

    local full_cli="loom-${cli_name}"

    # Try system-installed CLI first
    if command -v "$full_cli" >/dev/null 2>&1; then
        exec "$full_cli" "$@"
    fi

    # Try to find loom-tools directory
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"

    if find_loom_tools "$script_dir"; then
        # Try venv CLI
        if [[ -x "$LOOM_TOOLS_DIR/.venv/bin/$full_cli" ]]; then
            exec "$LOOM_TOOLS_DIR/.venv/bin/$full_cli" "$@"
        fi

        # Try Python module with PYTHONPATH
        if [[ -d "$LOOM_TOOLS_SRC/loom_tools" ]]; then
            PYTHONPATH="${LOOM_TOOLS_SRC}:${PYTHONPATH:-}" exec python3 -m "loom_tools.${module_name}" "$@"
        fi
    fi

    # Not found - provide helpful error message
    _loom_tool_not_found_error "$cli_name" "$module_name"
}

# Print helpful error message when loom-tools is not found
_loom_tool_not_found_error() {
    local cli_name="$1"
    local module_name="$2"
    local full_cli="loom-${cli_name}"

    echo "[ERROR] $full_cli not found." >&2
    echo "" >&2
    echo "The loom-tools package is required but not installed." >&2
    echo "" >&2
    echo "To install:" >&2

    local script_dir repo_root
    script_dir="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
    if repo_root="$(_find_repo_root "$script_dir")"; then
        if [[ -d "$repo_root/loom-tools" ]]; then
            echo "  cd $repo_root && pipx install --editable ./loom-tools" >&2
            echo "" >&2
            echo "Or using pip:" >&2
            echo "  pip install -e $repo_root/loom-tools" >&2
        elif [[ -f "$repo_root/.loom/loom-source-path" ]]; then
            local loom_source
            loom_source="$(cat "$repo_root/.loom/loom-source-path")"
            echo "  pipx install --editable $loom_source/loom-tools" >&2
        else
            echo "  pipx install loom-tools" >&2
        fi
    else
        echo "  pipx install loom-tools" >&2
    fi

    exit 1
}
