#!/usr/bin/env bash
# Check if GitHub CLI has workflow scope for modifying workflow files
#
# Returns:
#   0 - workflow scope is available
#   1 - workflow scope is NOT available
#   2 - error checking scope

set -euo pipefail

# Get auth status and check for workflow scope
AUTH_OUTPUT=$(gh auth status 2>&1) || {
  echo "Error: GitHub CLI is not authenticated" >&2
  exit 2
}

# Check if the workflow scope is listed
# gh auth status shows scopes like: Token scopes: 'gist', 'read:org', 'repo', 'workflow'
if echo "$AUTH_OUTPUT" | grep -qE "workflow"; then
  exit 0
else
  exit 1
fi
