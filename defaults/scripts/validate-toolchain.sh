#!/bin/bash
# validate-toolchain.sh - Validate loom-tools commands are available
#
# Validates that essential loom-tools commands are installed and accessible
# before the daemon enters its main loop. Provides tiered validation with
# critical vs optional commands.
#
# Exit codes:
#   0 - All critical commands available (optional warnings may exist)
#   1 - Critical commands missing (daemon cannot start)
#   2 - Invalid arguments
#
# Usage:
#   validate-toolchain.sh           # Validate all commands
#   validate-toolchain.sh --quick   # Only validate critical commands
#   validate-toolchain.sh --json    # JSON output for automation
#   validate-toolchain.sh --help    # Show help

set -euo pipefail

# Critical commands - daemon cannot function without these
CRITICAL_COMMANDS=(
    "loom-daemon-cleanup"
    "loom-recover-orphans"
    "loom-snapshot"
)

# Optional commands - daemon can continue with degraded functionality
OPTIONAL_COMMANDS=(
    "loom-stuck-detection"
    "loom-status"
    "loom-health-monitor"
    "loom-agent-wait"
    "loom-agent-spawn"
    "loom-validate-state"
    "loom-milestone"
)

# Colors for output
RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

# Output format
JSON_OUTPUT=false
QUICK_MODE=false

show_help() {
    cat << 'EOF'
validate-toolchain.sh - Validate loom-tools commands

USAGE:
    validate-toolchain.sh [OPTIONS]

OPTIONS:
    --quick     Only validate critical commands (faster)
    --json      Output results as JSON
    --help      Show this help message

CRITICAL COMMANDS (required):
    loom-daemon-cleanup   - Cleanup stale artifacts at daemon startup
    loom-recover-orphans  - Recover orphaned shepherds after crash
    loom-snapshot         - Generate pipeline snapshot for iteration

OPTIONAL COMMANDS (degraded without):
    loom-stuck-detection  - Detect stuck agents
    loom-status           - Show daemon status
    loom-health-monitor   - Health monitoring
    loom-agent-wait       - Wait for agent completion
    loom-agent-spawn      - Spawn agent sessions
    loom-validate-state   - Validate daemon state
    loom-milestone        - Report progress milestones

INSTALLATION:
    If commands are missing, install loom-tools:

    # From the repository root:
    pip install -e ./loom-tools

    # Or with uv (recommended):
    uv pip install -e ./loom-tools

    # Verify installation:
    which loom-daemon-cleanup

EXIT CODES:
    0 - All critical commands available
    1 - Critical commands missing
    2 - Invalid arguments

EXAMPLES:
    # Full validation
    validate-toolchain.sh

    # Quick check (critical only)
    validate-toolchain.sh --quick

    # JSON output for automation
    validate-toolchain.sh --json
EOF
}

# Check if a command exists
command_exists() {
    local cmd="$1"

    # First try: check if command is in PATH
    if command -v "$cmd" >/dev/null 2>&1; then
        return 0
    fi

    # Second try: check if Python module can be invoked
    # Map command names to module paths
    local module_name
    case "$cmd" in
        loom-daemon-cleanup) module_name="loom_tools.daemon_cleanup" ;;
        loom-recover-orphans) module_name="loom_tools.orphan_recovery" ;;
        loom-snapshot) module_name="loom_tools.snapshot" ;;
        loom-stuck-detection) module_name="loom_tools.stuck_detection" ;;
        loom-status) module_name="loom_tools.status" ;;
        loom-health-monitor) module_name="loom_tools.health_monitor" ;;
        loom-agent-wait) module_name="loom_tools.agent_wait" ;;
        loom-agent-spawn) module_name="loom_tools.agent_spawn" ;;
        loom-validate-state) module_name="loom_tools.validate_state" ;;
        loom-milestone) module_name="loom_tools.milestones" ;;
        *) return 1 ;;
    esac

    # Check if module can be imported
    if python3 -c "import $module_name" 2>/dev/null; then
        return 0
    fi

    return 1
}

# Get command location (for verbose output)
get_command_location() {
    local cmd="$1"

    if command -v "$cmd" >/dev/null 2>&1; then
        command -v "$cmd"
    else
        echo "python3 -m <module>"
    fi
}

# Main validation
main() {
    # Parse arguments
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --quick)
                QUICK_MODE=true
                shift
                ;;
            --json)
                JSON_OUTPUT=true
                shift
                ;;
            --help)
                show_help
                exit 0
                ;;
            *)
                echo "Unknown option: $1" >&2
                echo "Run 'validate-toolchain.sh --help' for usage" >&2
                exit 2
                ;;
        esac
    done

    local start_time
    start_time=$(date +%s%N 2>/dev/null || date +%s)

    local critical_missing=()
    local critical_found=()
    local optional_missing=()
    local optional_found=()

    # Validate critical commands
    for cmd in "${CRITICAL_COMMANDS[@]}"; do
        if command_exists "$cmd"; then
            critical_found+=("$cmd")
        else
            critical_missing+=("$cmd")
        fi
    done

    # Validate optional commands (unless quick mode)
    if [[ "$QUICK_MODE" != "true" ]]; then
        for cmd in "${OPTIONAL_COMMANDS[@]}"; do
            if command_exists "$cmd"; then
                optional_found+=("$cmd")
            else
                optional_missing+=("$cmd")
            fi
        done
    fi

    local end_time
    end_time=$(date +%s%N 2>/dev/null || date +%s)

    # Calculate duration (handle both nanosecond and second precision)
    local duration_ms
    if [[ "$start_time" =~ ^[0-9]{10,}$ ]]; then
        # Nanosecond precision available
        duration_ms=$(( (end_time - start_time) / 1000000 ))
    else
        # Only second precision
        duration_ms=$(( (end_time - start_time) * 1000 ))
    fi

    # Determine overall status
    local status="ok"
    local exit_code=0
    if [[ ${#critical_missing[@]} -gt 0 ]]; then
        status="critical"
        exit_code=1
    elif [[ ${#optional_missing[@]} -gt 0 ]]; then
        status="degraded"
    fi

    # Output results - handle empty arrays carefully
    local cf_str="" cm_str="" of_str="" om_str=""
    [[ ${#critical_found[@]} -gt 0 ]] && cf_str="${critical_found[*]}"
    [[ ${#critical_missing[@]} -gt 0 ]] && cm_str="${critical_missing[*]}"
    [[ ${#optional_found[@]} -gt 0 ]] && of_str="${optional_found[*]}"
    [[ ${#optional_missing[@]} -gt 0 ]] && om_str="${optional_missing[*]}"

    if [[ "$JSON_OUTPUT" == "true" ]]; then
        local cf_json="[]" cm_json="[]" of_json="[]" om_json="[]"
        [[ ${#critical_found[@]} -gt 0 ]] && cf_json="$(printf '%s\n' "${critical_found[@]}" | jq -R . | jq -s .)"
        [[ ${#critical_missing[@]} -gt 0 ]] && cm_json="$(printf '%s\n' "${critical_missing[@]}" | jq -R . | jq -s .)"
        [[ ${#optional_found[@]} -gt 0 ]] && of_json="$(printf '%s\n' "${optional_found[@]}" | jq -R . | jq -s .)"
        [[ ${#optional_missing[@]} -gt 0 ]] && om_json="$(printf '%s\n' "${optional_missing[@]}" | jq -R . | jq -s .)"
        output_json "$status" "$duration_ms" "$cf_json" "$cm_json" "$of_json" "$om_json"
    else
        output_text "$status" "$duration_ms" "$cf_str" "$cm_str" "$of_str" "$om_str"
    fi

    exit "$exit_code"
}

output_json() {
    local status="$1"
    local duration_ms="$2"
    local critical_found="$3"
    local critical_missing="$4"
    local optional_found="$5"
    local optional_missing="$6"

    # Handle empty arrays
    [[ -z "$critical_found" || "$critical_found" == "[]" ]] && critical_found="[]"
    [[ -z "$critical_missing" || "$critical_missing" == "[]" ]] && critical_missing="[]"
    [[ -z "$optional_found" || "$optional_found" == "[]" ]] && optional_found="[]"
    [[ -z "$optional_missing" || "$optional_missing" == "[]" ]] && optional_missing="[]"

    cat << EOF
{
  "status": "$status",
  "duration_ms": $duration_ms,
  "critical": {
    "found": $critical_found,
    "missing": $critical_missing
  },
  "optional": {
    "found": $optional_found,
    "missing": $optional_missing
  }
}
EOF
}

output_text() {
    local status="$1"
    local duration_ms="$2"
    local critical_found="$3"
    local critical_missing="$4"
    local optional_found="$5"
    local optional_missing="$6"

    echo "Loom Toolchain Validation"
    echo "========================="
    echo ""

    # Critical commands
    echo "Critical commands:"
    if [[ -n "$critical_found" ]]; then
        for cmd in $critical_found; do
            echo -e "  ${GREEN}✓${NC} $cmd"
        done
    fi
    if [[ -n "$critical_missing" ]]; then
        for cmd in $critical_missing; do
            echo -e "  ${RED}✗${NC} $cmd (MISSING)"
        done
    fi
    echo ""

    # Optional commands (if checked)
    if [[ "$QUICK_MODE" != "true" ]]; then
        echo "Optional commands:"
        if [[ -n "$optional_found" ]]; then
            for cmd in $optional_found; do
                echo -e "  ${GREEN}✓${NC} $cmd"
            done
        fi
        if [[ -n "$optional_missing" ]]; then
            for cmd in $optional_missing; do
                echo -e "  ${YELLOW}○${NC} $cmd (optional, degraded functionality)"
            done
        fi
        echo ""
    fi

    # Summary
    echo "---"
    echo "Validation completed in ${duration_ms}ms"

    case "$status" in
        ok)
            echo -e "${GREEN}Status: OK${NC} - All commands available"
            ;;
        degraded)
            echo -e "${YELLOW}Status: DEGRADED${NC} - Optional commands missing"
            echo ""
            echo "The daemon will continue with degraded functionality."
            echo "Some features (stuck detection, health monitoring) may not work."
            ;;
        critical)
            echo -e "${RED}Status: CRITICAL${NC} - Essential commands missing"
            echo ""
            echo "The daemon cannot start without these commands."
            echo ""
            echo "To install loom-tools, run:"
            echo "  pip install -e ./loom-tools"
            echo ""
            echo "Or with uv:"
            echo "  uv pip install -e ./loom-tools"
            ;;
    esac
}

main "$@"
