#!/usr/bin/env bash
# Setup branch protection rules for Loom workflow
#
# Usage:
#   ./scripts/install/setup-branch-protection.sh /path/to/target-repo [branch-name]
#
# Applies recommended branch protection rules:
#   - Require PRs (0 approvals for solo dev/Loom workflows)
#   - Dismiss stale reviews
#   - Prevent force pushes and deletions

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

if [[ -z "$TARGET_PATH" ]]; then
  error "Target path required"
  echo "Usage: $0 /path/to/target-repo [branch-name]"
  exit 1
fi

cd "$TARGET_PATH"

# Get repo owner/name
REPO_INFO=$(gh repo view --json owner,name 2>/dev/null || true)
if [[ -z "$REPO_INFO" ]]; then
  error "Failed to get repository information. Is this a GitHub repository?"
  exit 1
fi

OWNER=$(echo "$REPO_INFO" | jq -r .owner.login)
REPO=$(echo "$REPO_INFO" | jq -r .name)

echo ""
info "Configuring branch protection for: ${OWNER}/${REPO} (${BRANCH_NAME})"

# Check if user has admin permissions
HAS_ADMIN=$(gh api "repos/${OWNER}/${REPO}" --jq '.permissions.admin' 2>/dev/null || echo "false")
if [[ "$HAS_ADMIN" != "true" ]]; then
  warning "You may not have admin permissions to configure branch protection"
  warning "Attempting anyway (may fail with permission error)..."
fi

# Apply branch protection rules using JSON payload
# Note: Using --input with JSON is more reliable than --field with bracket notation
# Note: required_approving_review_count is 0 to support solo development and Loom workflows
#       (GitHub's review API doesn't work for self-authored PRs, and Loom uses label-based reviews)
if gh api --method PUT "repos/${OWNER}/${REPO}/branches/${BRANCH_NAME}/protection" --input - > /dev/null 2>&1 <<'EOF'
{
  "required_status_checks": null,
  "enforce_admins": false,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 0
  },
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_linear_history": false
}
EOF
then

  success "Branch protection configured successfully"
  echo ""
  echo "Applied rules:"
  echo "  - Require pull requests before merging (0 approvals required)"
  echo "  - Dismiss stale reviews on new commits"
  echo "  - Prevent force pushes"
  echo "  - Prevent branch deletion"
  echo "  - Admins can bypass (enforce_admins=false)"
  echo ""
  echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
  echo ""
  info "To modify: GitHub Settings > Branches > ${BRANCH_NAME}"
  exit 0
else
  error "Failed to configure branch protection"
  echo ""
  echo "This can happen if:"
  echo "  - You lack admin permissions on ${OWNER}/${REPO}"
  echo "  - The branch '${BRANCH_NAME}' does not exist yet"
  echo "  - GitHub API is unreachable"
  echo ""
  info "To configure manually:"
  echo "  1. Go to: https://github.com/${OWNER}/${REPO}/settings/branches"
  echo "  2. Add rule for '${BRANCH_NAME}' branch"
  echo "  3. Enable: Require pull request reviews (0 approvals)"
  echo "  4. Enable: Dismiss stale reviews"
  echo "  5. Enable: Prevent force pushes"
  exit 1
fi
