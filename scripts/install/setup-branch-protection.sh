#!/usr/bin/env bash
# Setup branch rulesets for Loom workflow
#
# Usage:
#   ./scripts/install/setup-branch-protection.sh /path/to/target-repo [branch-name]
#
# Creates or updates a GitHub ruleset with recommended rules:
#   - Prevent branch deletion and force pushes
#   - Require linear history (squash merges only)
#   - Require pull requests (0 approvals for solo dev/Loom workflows)

set -euo pipefail

# Source helper functions if available
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "${SCRIPT_DIR}/../install-loom.sh" ]]; then
  # Define helper functions for consistent output
  info() { echo -e "\033[0;34mℹ $*\033[0m"; }
  success() { echo -e "\033[0;32m✓ $*\033[0m"; }
  warning() { echo -e "\033[1;33m⚠ $*\033[0m"; }
  error() { echo -e "\033[0;31m✗ $*\033[0m"; }
fi

TARGET_PATH="${1:-}"
BRANCH_NAME="${2:-main}"
RULESET_NAME="main"

if [[ -z "$TARGET_PATH" ]]; then
  error "Target path required"
  echo "Usage: $0 /path/to/target-repo [branch-name]"
  exit 1
fi

cd "$TARGET_PATH"

# Detect the repository from origin remote (not upstream)
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Failed to get repository information. Is this a GitHub repository?"
  exit 1
fi

# Extract owner/repo from URL (handles both HTTPS and SSH)
REPO_NAME=$(echo "$ORIGIN_URL" | sed -E 's#^.*(github\.com[/:])##; s/\.git$//')

if [[ ! "$REPO_NAME" =~ ^[^/]+/[^/]+$ ]]; then
  error "Could not extract valid repository from URL: $ORIGIN_URL"
  exit 1
fi

OWNER=$(echo "$REPO_NAME" | cut -d'/' -f1)
REPO=$(echo "$REPO_NAME" | cut -d'/' -f2)

echo ""
info "Configuring branch ruleset for: ${OWNER}/${REPO} (${BRANCH_NAME})"

# Check if user has admin permissions
HAS_ADMIN=$(gh api "repos/${OWNER}/${REPO}" --jq '.permissions.admin' 2>/dev/null || echo "false")
if [[ "$HAS_ADMIN" != "true" ]]; then
  warning "You may not have admin permissions to configure rulesets"
  warning "Attempting anyway (may fail with permission error)..."
fi

# Ruleset payload
RULESET_PAYLOAD='{
  "name": "'"$RULESET_NAME"'",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": {
      "include": ["~DEFAULT_BRANCH"],
      "exclude": []
    }
  },
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"},
    {"type": "required_linear_history"},
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": false,
        "allowed_merge_methods": ["squash"]
      }
    }
  ]
}'

# Check if a ruleset named "main" already exists
EXISTING_ID=$(gh api "repos/${OWNER}/${REPO}/rulesets" --jq '.[] | select(.name == "'"$RULESET_NAME"'") | .id' 2>/dev/null || echo "")

if [[ -n "$EXISTING_ID" ]]; then
  info "Found existing ruleset '${RULESET_NAME}' (ID: ${EXISTING_ID}), updating..."
  API_METHOD="PUT"
  API_URL="repos/${OWNER}/${REPO}/rulesets/${EXISTING_ID}"
else
  info "Creating new ruleset '${RULESET_NAME}'..."
  API_METHOD="POST"
  API_URL="repos/${OWNER}/${REPO}/rulesets"
fi

if echo "$RULESET_PAYLOAD" | gh api --method "$API_METHOD" "$API_URL" --input - > /dev/null 2>&1; then
  success "Branch ruleset configured successfully"
  echo ""
  echo "Applied rules:"
  echo "  - Prevent branch deletion"
  echo "  - Prevent force pushes"
  echo "  - Require linear history (squash merges only)"
  echo "  - Require pull requests (0 approvals required)"
  echo "  - Dismiss stale reviews on new commits"
  echo ""
  echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
  echo ""
  info "To modify: GitHub Settings > Rules > Rulesets"
  exit 0
else
  error "Failed to configure branch ruleset"
  echo ""
  echo "This can happen if:"
  echo "  - You lack admin permissions on ${OWNER}/${REPO}"
  echo "  - GitHub API is unreachable"
  echo ""
  info "To configure manually:"
  echo "  1. Go to: https://github.com/${OWNER}/${REPO}/settings/rules"
  echo "  2. Create a new ruleset for the default branch"
  echo "  3. Enable: Prevent deletion, prevent force push"
  echo "  4. Enable: Require linear history"
  echo "  5. Enable: Require pull request (0 approvals)"
  exit 1
fi
