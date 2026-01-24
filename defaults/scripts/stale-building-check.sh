#!/usr/bin/env bash
# Detect and recover orphaned issues stuck in loom:building state
#
# This script finds issues that have been in loom:building state for too long
# without an associated PR, indicating the agent that claimed them has crashed
# or been cancelled.
#
# Usage:
#   ./stale-building-check.sh              # Check for stale issues (dry run)
#   ./stale-building-check.sh --recover    # Reset stale issues to loom:issue
#   ./stale-building-check.sh --json       # Output JSON for programmatic use
#
# Thresholds (configurable via environment):
#   STALE_THRESHOLD_HOURS=2    # Hours before issue is considered stale
#   STALE_WITH_PR_HOURS=24     # Hours before issue with stale PR is flagged

set -euo pipefail

# Configuration
STALE_THRESHOLD_HOURS="${STALE_THRESHOLD_HOURS:-2}"
STALE_WITH_PR_HOURS="${STALE_WITH_PR_HOURS:-24}"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Parse arguments
RECOVER=false
JSON_OUTPUT=false
VERBOSE=false

while [[ $# -gt 0 ]]; do
  case $1 in
    --recover)
      RECOVER=true
      shift
      ;;
    --json)
      JSON_OUTPUT=true
      shift
      ;;
    --verbose|-v)
      VERBOSE=true
      shift
      ;;
    --help|-h)
      echo "Usage: $0 [OPTIONS]"
      echo ""
      echo "Detect and recover orphaned issues stuck in loom:building state."
      echo ""
      echo "Options:"
      echo "  --recover    Reset stale issues to loom:issue state"
      echo "  --json       Output JSON for programmatic use"
      echo "  --verbose    Show detailed progress"
      echo "  --help       Show this help message"
      echo ""
      echo "Environment variables:"
      echo "  STALE_THRESHOLD_HOURS    Hours before no-PR issue is stale (default: 2)"
      echo "  STALE_WITH_PR_HOURS      Hours before stale-PR issue is flagged (default: 24)"
      exit 0
      ;;
    *)
      echo "Unknown option: $1"
      exit 1
      ;;
  esac
done

log_info() {
  if [[ "$JSON_OUTPUT" != "true" ]]; then
    echo -e "${BLUE}ℹ $*${NC}" >&2
  fi
}

log_warn() {
  if [[ "$JSON_OUTPUT" != "true" ]]; then
    echo -e "${YELLOW}⚠ $*${NC}" >&2
  fi
}

log_error() {
  if [[ "$JSON_OUTPUT" != "true" ]]; then
    echo -e "${RED}✗ $*${NC}" >&2
  fi
}

log_success() {
  if [[ "$JSON_OUTPUT" != "true" ]]; then
    echo -e "${GREEN}✓ $*${NC}" >&2
  fi
}

# Get current timestamp
NOW=$(date +%s)

# Calculate threshold in seconds
STALE_THRESHOLD_SECS=$((STALE_THRESHOLD_HOURS * 3600))
STALE_WITH_PR_SECS=$((STALE_WITH_PR_HOURS * 3600))

log_info "Checking for stale loom:building issues..."
log_info "Threshold (no PR): ${STALE_THRESHOLD_HOURS} hours"
log_info "Threshold (with PR): ${STALE_WITH_PR_HOURS} hours"

# Get all loom:building issues with their creation/update times
BUILDING_ISSUES=$(gh issue list --label "loom:building" --state open --json number,title,createdAt,updatedAt 2>/dev/null || echo "[]")

if [[ "$BUILDING_ISSUES" == "[]" ]]; then
  log_success "No loom:building issues found"
  if [[ "$JSON_OUTPUT" == "true" ]]; then
    echo '{"stale_issues":[],"total_building":0}'
  fi
  exit 0
fi

TOTAL_BUILDING=$(echo "$BUILDING_ISSUES" | jq 'length')
log_info "Found $TOTAL_BUILDING issues with loom:building label"

# Get all open PRs once (more efficient than per-issue queries)
OPEN_PRS=$(gh pr list --state open --json number,headRefName,body 2>/dev/null || echo "[]")

# Collect stale issues using a temp file to avoid subshell issues
STALE_FILE=$(mktemp)
echo "[]" > "$STALE_FILE"

# Process each issue
ISSUE_COUNT=$(echo "$BUILDING_ISSUES" | jq 'length')
for i in $(seq 0 $((ISSUE_COUNT - 1))); do
  issue=$(echo "$BUILDING_ISSUES" | jq -c ".[$i]")
  NUMBER=$(echo "$issue" | jq -r '.number')
  TITLE=$(echo "$issue" | jq -r '.title')
  UPDATED_AT=$(echo "$issue" | jq -r '.updatedAt')

  # Convert ISO timestamp to epoch (macOS vs Linux compatibility)
  if [[ "$(uname)" == "Darwin" ]]; then
    # macOS: Handle different timestamp formats
    UPDATED_EPOCH=$(date -j -f "%Y-%m-%dT%H:%M:%SZ" "$UPDATED_AT" +%s 2>/dev/null || \
                    date -j -f "%Y-%m-%dT%H:%M:%S" "${UPDATED_AT%Z}" +%s 2>/dev/null || echo "0")
  else
    # Linux
    UPDATED_EPOCH=$(date -d "$UPDATED_AT" +%s 2>/dev/null || echo "0")
  fi

  if [[ "$UPDATED_EPOCH" == "0" ]]; then
    log_warn "Could not parse date for issue #$NUMBER, skipping"
    continue
  fi

  # Calculate age in hours
  AGE_SECS=$((NOW - UPDATED_EPOCH))
  AGE_HOURS=$((AGE_SECS / 3600))

  [[ "$VERBOSE" == "true" ]] && log_info "Checking #$NUMBER (updated ${AGE_HOURS}h ago)"

  # Check if there's an open PR for this issue
  # Search for PRs that reference this issue number in body or branch name
  OPEN_PR=$(echo "$OPEN_PRS" | jq --arg num "$NUMBER" \
    '[.[] | select(
      (.body // "" | test("(Closes|Fixes|Resolves) #" + $num + "\\b"; "i")) or
      (.headRefName | test("issue-" + $num + "\\b"))
    )] | first // empty' 2>/dev/null || echo "")

  if [[ -z "$OPEN_PR" || "$OPEN_PR" == "null" ]]; then
    # No open PR found
    if [[ $AGE_SECS -gt $STALE_THRESHOLD_SECS ]]; then
      log_warn "#$NUMBER: No PR found after ${AGE_HOURS}h - STALE"

      if [[ "$RECOVER" == "true" ]]; then
        log_info "Recovering #$NUMBER..."
        gh issue edit "$NUMBER" --remove-label "loom:building" --add-label "loom:issue"

        # Create comment body
        COMMENT_BODY="🔄 **Auto-recovered from stale state**

This issue was in \`loom:building\` state for ${AGE_HOURS} hours without an associated PR.

**What happened:**
- An agent claimed this issue but didn't create a PR
- The agent may have crashed, timed out, or been cancelled
- The stale-building-check script detected this orphaned state

**Action taken:**
- Removed \`loom:building\` label
- Added \`loom:issue\` label to make it available for work again

This issue is now ready to be claimed by another agent."

        gh issue comment "$NUMBER" --body "$COMMENT_BODY"
        log_success "Recovered #$NUMBER"
      fi

      # Add to stale list
      CURRENT=$(cat "$STALE_FILE")
      echo "$CURRENT" | jq --arg num "$NUMBER" --arg title "$TITLE" --argjson age "$AGE_HOURS" --arg reason "no_pr" \
        '. + [{"number": ($num | tonumber), "title": $title, "age_hours": $age, "reason": $reason, "has_pr": false}]' > "$STALE_FILE"
    fi
  else
    # Has open PR - check if PR is also stale
    PR_NUMBER=$(echo "$OPEN_PR" | jq -r '.number // empty')
    if [[ -n "$PR_NUMBER" && $AGE_SECS -gt $STALE_WITH_PR_SECS ]]; then
      log_warn "#$NUMBER: PR #$PR_NUMBER exists but no progress for ${AGE_HOURS}h"

      # Don't auto-recover issues with PRs - they need manual review
      CURRENT=$(cat "$STALE_FILE")
      echo "$CURRENT" | jq --arg num "$NUMBER" --arg title "$TITLE" --argjson age "$AGE_HOURS" --arg reason "stale_pr" --arg pr "$PR_NUMBER" \
        '. + [{"number": ($num | tonumber), "title": $title, "age_hours": $age, "reason": $reason, "has_pr": true, "pr_number": ($pr | tonumber)}]' > "$STALE_FILE"
    else
      [[ "$VERBOSE" == "true" ]] && log_success "#$NUMBER: Has active PR #$PR_NUMBER"
    fi
  fi
done

# Read final stale issues
STALE_ISSUES=$(cat "$STALE_FILE")
rm -f "$STALE_FILE"

# Output results
STALE_COUNT=$(echo "$STALE_ISSUES" | jq 'length')

if [[ "$JSON_OUTPUT" == "true" ]]; then
  echo "$STALE_ISSUES" | jq --argjson total "$TOTAL_BUILDING" '{stale_issues: ., total_building: $total, stale_count: (. | length)}'
else
  echo ""
  if [[ "$STALE_COUNT" -gt 0 ]]; then
    log_warn "Found $STALE_COUNT stale issues out of $TOTAL_BUILDING in loom:building state"
    echo ""
    echo "Stale issues:"
    echo "$STALE_ISSUES" | jq -r '.[] | "  #\(.number): \(.title) (\(.age_hours)h, \(.reason))"'

    if [[ "$RECOVER" != "true" ]]; then
      echo ""
      echo "Run with --recover to reset stale issues to loom:issue state"
    fi
  else
    log_success "All $TOTAL_BUILDING loom:building issues are active"
  fi
fi

exit 0
