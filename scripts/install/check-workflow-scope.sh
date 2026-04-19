#!/usr/bin/env bash
# Check if GitHub CLI has workflow scope for modifying workflow files
#
# For Gitea forges, this check is not applicable and always returns success.
#
# Returns:
#   0 - workflow scope is available (or not applicable for Gitea)
#   1 - workflow scope is NOT available
#   2 - error checking scope

set -euo pipefail

# Source forge detection helper if available
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "${SCRIPT_DIR}/forge-detect.sh" ]]; then
  source "${SCRIPT_DIR}/forge-detect.sh"

  # Try to detect forge from current directory's git remote
  ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
  if [[ -n "$ORIGIN_URL" ]]; then
    if detect_forge_and_repo "$ORIGIN_URL" 2>/dev/null; then
      if [[ "$FORGE_TYPE" == "gitea" ]]; then
        # Workflow scope check is not applicable for Gitea
        exit 0
      fi
    fi
  fi
fi

# GitHub: check for workflow scope
AUTH_OUTPUT=$(gh auth status 2>&1) || {
  echo "Error: GitHub CLI is not authenticated" >&2
  exit 2
}

# Check if the workflow scope is listed
if echo "$AUTH_OUTPUT" | grep -qE "workflow"; then
  exit 0
else
  exit 1
fi
