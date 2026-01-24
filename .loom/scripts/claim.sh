#!/bin/bash

# Loom Claim Helper Script
# Atomic claiming system for parallel agent coordination
#
# Usage:
#   ./.loom/scripts/claim.sh claim <issue-number> [agent-id] [ttl-seconds]
#   ./.loom/scripts/claim.sh release <issue-number> [agent-id]
#   ./.loom/scripts/claim.sh check <issue-number>
#   ./.loom/scripts/claim.sh list
#   ./.loom/scripts/claim.sh --help

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
CLAIMS_DIR=".loom/claims"
DEFAULT_TTL=3600  # 1 hour

# Function to print colored output
print_error() {
    echo -e "${RED}ERROR: $1${NC}" >&2
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

# Function to ensure claims directory exists
ensure_claims_dir() {
    if [[ ! -d "$CLAIMS_DIR" ]]; then
        mkdir -p "$CLAIMS_DIR"
    fi
}

# Function to get current timestamp in ISO format
get_timestamp() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

# Function to get Unix timestamp
get_unix_timestamp() {
    date +%s
}

# Function to generate default agent ID
generate_agent_id() {
    echo "agent-$$-$(hostname | tr '.' '-' | cut -c1-8)"
}

# Function to show help
show_help() {
    cat << 'EOF'
Loom Claim Helper

This script provides atomic claiming for parallel agent coordination.
Claims are stored as directories in .loom/claims/ to ensure atomic operations.

Usage:
  ./.loom/scripts/claim.sh claim <issue-number> [agent-id] [ttl-seconds]
  ./.loom/scripts/claim.sh release <issue-number> [agent-id]
  ./.loom/scripts/claim.sh check <issue-number>
  ./.loom/scripts/claim.sh list
  ./.loom/scripts/claim.sh --help

Commands:
  claim     Attempt to claim an issue atomically
            - agent-id: Optional, auto-generated if not provided
            - ttl-seconds: Optional, defaults to 3600 (1 hour)

  release   Release a claim on an issue
            - agent-id: Optional, validates ownership if provided

  check     Check if an issue is claimed
            - Returns claim metadata if claimed, exits 1 if not

  list      List all current claims

Examples:
  ./.loom/scripts/claim.sh claim 123
    Claim issue #123 with auto-generated agent ID

  ./.loom/scripts/claim.sh claim 123 builder-1 7200
    Claim issue #123 as builder-1 with 2-hour TTL

  ./.loom/scripts/claim.sh release 123
    Release claim on issue #123

  ./.loom/scripts/claim.sh check 123
    Check if issue #123 is claimed

  ./.loom/scripts/claim.sh list
    Show all current claims

How It Works:
  Claims use mkdir for atomicity - only one process can successfully
  create a directory. This prevents race conditions when multiple
  agents try to claim the same issue.

Claim Metadata:
  Each claim stores a claim.json file with:
  - issue_number: The issue being claimed
  - agent_id: ID of the claiming agent
  - created_at: When the claim was made
  - expires_at: When the claim expires
  - ttl: Time-to-live in seconds

Notes:
  - Claims are stored in .loom/claims/issue-<N>.lock/
  - The .lock directories are gitignored
  - Expired claims can be cleaned with: claim.sh cleanup
  - Use release to explicitly free a claim
EOF
}

# Function to claim an issue
do_claim() {
    local issue_number="$1"
    local agent_id="${2:-$(generate_agent_id)}"
    local ttl="${3:-$DEFAULT_TTL}"

    # Validate issue number
    if ! [[ "$issue_number" =~ ^[0-9]+$ ]]; then
        print_error "Issue number must be numeric (got: '$issue_number')"
        exit 1
    fi

    # Validate TTL
    if ! [[ "$ttl" =~ ^[0-9]+$ ]]; then
        print_error "TTL must be numeric seconds (got: '$ttl')"
        exit 1
    fi

    ensure_claims_dir

    local claim_dir="$CLAIMS_DIR/issue-${issue_number}.lock"
    local claim_file="$claim_dir/claim.json"

    # Calculate expiration
    local created_at=$(get_timestamp)
    local created_unix=$(get_unix_timestamp)
    local expires_unix=$((created_unix + ttl))
    local expires_at=$(date -u -r "$expires_unix" +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date -u -d "@$expires_unix" +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || echo "unknown")

    # Attempt atomic claim via mkdir
    if mkdir "$claim_dir" 2>/dev/null; then
        # Successfully claimed - write metadata
        cat > "$claim_file" << EOF
{
  "issue_number": $issue_number,
  "agent_id": "$agent_id",
  "created_at": "$created_at",
  "expires_at": "$expires_at",
  "expires_unix": $expires_unix,
  "ttl": $ttl
}
EOF
        print_success "Claimed issue #$issue_number"
        print_info "Agent: $agent_id"
        print_info "Expires: $expires_at (TTL: ${ttl}s)"
        exit 0
    else
        # Claim failed - check if already claimed
        if [[ -f "$claim_file" ]]; then
            local existing_agent=$(grep -o '"agent_id": *"[^"]*"' "$claim_file" | cut -d'"' -f4)
            print_error "Issue #$issue_number is already claimed by: $existing_agent"
            exit 1
        else
            print_error "Failed to claim issue #$issue_number (directory exists but no metadata)"
            exit 1
        fi
    fi
}

# Function to release a claim
do_release() {
    local issue_number="$1"
    local agent_id="$2"

    # Validate issue number
    if ! [[ "$issue_number" =~ ^[0-9]+$ ]]; then
        print_error "Issue number must be numeric (got: '$issue_number')"
        exit 1
    fi

    local claim_dir="$CLAIMS_DIR/issue-${issue_number}.lock"
    local claim_file="$claim_dir/claim.json"

    # Check if claim exists
    if [[ ! -d "$claim_dir" ]]; then
        print_warning "Issue #$issue_number is not claimed"
        exit 0
    fi

    # Validate ownership if agent_id provided
    if [[ -n "$agent_id" ]] && [[ -f "$claim_file" ]]; then
        local existing_agent=$(grep -o '"agent_id": *"[^"]*"' "$claim_file" | cut -d'"' -f4)
        if [[ "$existing_agent" != "$agent_id" ]]; then
            print_error "Cannot release: issue #$issue_number is claimed by '$existing_agent', not '$agent_id'"
            exit 1
        fi
    fi

    # Release the claim
    if rm -rf "$claim_dir"; then
        print_success "Released claim on issue #$issue_number"
        exit 0
    else
        print_error "Failed to release claim on issue #$issue_number"
        exit 1
    fi
}

# Function to check if an issue is claimed
do_check() {
    local issue_number="$1"

    # Validate issue number
    if ! [[ "$issue_number" =~ ^[0-9]+$ ]]; then
        print_error "Issue number must be numeric (got: '$issue_number')"
        exit 1
    fi

    local claim_dir="$CLAIMS_DIR/issue-${issue_number}.lock"
    local claim_file="$claim_dir/claim.json"

    if [[ -f "$claim_file" ]]; then
        print_info "Issue #$issue_number is claimed:"
        cat "$claim_file"
        echo ""
        exit 0
    else
        print_info "Issue #$issue_number is not claimed"
        exit 1
    fi
}

# Function to list all claims
do_list() {
    ensure_claims_dir

    local claims_found=0

    echo "Current claims:"
    echo ""

    for claim_dir in "$CLAIMS_DIR"/issue-*.lock; do
        if [[ -d "$claim_dir" ]]; then
            local claim_file="$claim_dir/claim.json"
            if [[ -f "$claim_file" ]]; then
                claims_found=$((claims_found + 1))
                local issue_num=$(grep -o '"issue_number": *[0-9]*' "$claim_file" | grep -o '[0-9]*')
                local agent_id=$(grep -o '"agent_id": *"[^"]*"' "$claim_file" | cut -d'"' -f4)
                local expires_at=$(grep -o '"expires_at": *"[^"]*"' "$claim_file" | cut -d'"' -f4)
                local expires_unix=$(grep -o '"expires_unix": *[0-9]*' "$claim_file" | grep -o '[0-9]*')
                local current_unix=$(get_unix_timestamp)

                local status="active"
                if [[ -n "$expires_unix" ]] && [[ "$current_unix" -gt "$expires_unix" ]]; then
                    status="EXPIRED"
                fi

                printf "  #%-6s %-20s expires: %-25s [%s]\n" "$issue_num" "$agent_id" "$expires_at" "$status"
            fi
        fi
    done

    if [[ $claims_found -eq 0 ]]; then
        echo "  (no active claims)"
    fi

    echo ""
    echo "Total: $claims_found claim(s)"
}

# Parse arguments
if [[ $# -eq 0 ]] || [[ "$1" == "--help" ]] || [[ "$1" == "-h" ]]; then
    show_help
    exit 0
fi

COMMAND="$1"
shift

case "$COMMAND" in
    claim)
        if [[ $# -lt 1 ]]; then
            print_error "claim requires an issue number"
            echo "Usage: ./.loom/scripts/claim.sh claim <issue-number> [agent-id] [ttl-seconds]"
            exit 1
        fi
        do_claim "$@"
        ;;
    release)
        if [[ $# -lt 1 ]]; then
            print_error "release requires an issue number"
            echo "Usage: ./.loom/scripts/claim.sh release <issue-number> [agent-id]"
            exit 1
        fi
        do_release "$@"
        ;;
    check)
        if [[ $# -lt 1 ]]; then
            print_error "check requires an issue number"
            echo "Usage: ./.loom/scripts/claim.sh check <issue-number>"
            exit 1
        fi
        do_check "$@"
        ;;
    list)
        do_list
        ;;
    *)
        print_error "Unknown command: $COMMAND"
        echo "Use --help for usage information"
        exit 1
        ;;
esac
