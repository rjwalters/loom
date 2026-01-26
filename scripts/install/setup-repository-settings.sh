#!/usr/bin/env bash
# Setup repository settings for Loom workflow
#
# Usage:
#   ./scripts/install/setup-repository-settings.sh /path/to/target-repo [--dry-run]
#
# Configures:
#   - Merge strategy (squash merge only)
#   - Auto-delete head branches
#   - Allow auto-merge
#   - Suggest updating branches

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

# Parse arguments
TARGET_PATH=""
DRY_RUN=false

while [[ $# -gt 0 ]]; do
  case $1 in
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    *)
      TARGET_PATH="$1"
      shift
      ;;
  esac
done

if [[ -z "$TARGET_PATH" ]]; then
  error "Target path required"
  echo "Usage: $0 /path/to/target-repo [--dry-run]"
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
# HTTPS: https://github.com/owner/repo.git -> owner/repo
# SSH: git@github.com:owner/repo.git -> owner/repo
REPO_NAME=$(echo "$ORIGIN_URL" | sed -E 's#^.*(github\.com[/:])##; s/\.git$//')

if [[ ! "$REPO_NAME" =~ ^[^/]+/[^/]+$ ]]; then
  error "Could not extract valid repository from URL: $ORIGIN_URL"
  exit 1
fi

OWNER=$(echo "$REPO_NAME" | cut -d'/' -f1)
REPO=$(echo "$REPO_NAME" | cut -d'/' -f2)

echo ""
info "Configuring repository settings for: ${OWNER}/${REPO}"

# Check if user has admin permissions
HAS_ADMIN=$(gh api "repos/${OWNER}/${REPO}" --jq '.permissions.admin' 2>/dev/null || echo "false")
if [[ "$HAS_ADMIN" != "true" ]]; then
  warning "You may not have admin permissions to configure repository settings"
  warning "Attempting anyway (may fail with permission error)..."
fi

# Define the settings to apply
SETTINGS_JSON='{
  "allow_merge_commit": false,
  "allow_squash_merge": true,
  "allow_rebase_merge": false,
  "delete_branch_on_merge": true,
  "allow_auto_merge": true,
  "allow_update_branch": true
}'

# In dry-run mode, just display what would be changed
if [[ "$DRY_RUN" == "true" ]]; then
  echo ""
  info "DRY RUN - Would apply the following settings:"
  echo ""
  echo "  Repository: ${OWNER}/${REPO}"
  echo ""
  echo "  Settings to be configured:"
  echo "    allow_merge_commit: false (disabled)"
  echo "    allow_squash_merge: true (default strategy - flattens PR to single commit)"
  echo "    allow_rebase_merge: false (disabled)"
  echo "    delete_branch_on_merge: true (auto-cleanup branches)"
  echo "    allow_auto_merge: true (enables Champion auto-merge)"
  echo "    allow_update_branch: true (suggest branch updates)"
  echo ""
  info "No changes made (dry-run mode)"
  exit 0
fi

# Apply repository settings using GitHub API
if gh api "repos/${OWNER}/${REPO}" -X PATCH --input - > /dev/null 2>&1 <<EOF
${SETTINGS_JSON}
EOF
then
  success "Repository settings configured successfully"
  echo ""
  echo "Applied settings:"
  echo "  - Allow merge commits: No (disabled)"
  echo "  - Allow squash merging: Yes (default strategy - flattens PR to single commit)"
  echo "  - Allow rebase merging: No (disabled)"
  echo "  - Delete branches on merge: Yes (auto-cleanup)"
  echo "  - Allow auto-merge: Yes (enables Champion workflow)"
  echo "  - Suggest updating branches: Yes"
  echo ""
  info "To modify: GitHub Settings > General > Pull Requests"
  exit 0
else
  error "Failed to configure repository settings"
  echo ""
  echo "This can happen if:"
  echo "  - You lack admin permissions on ${OWNER}/${REPO}"
  echo "  - GitHub API is unreachable"
  echo ""
  info "To configure manually:"
  echo "  1. Go to: https://github.com/${OWNER}/${REPO}/settings"
  echo "  2. Scroll to 'Pull Requests' section"
  echo "  3. Configure merge options:"
  echo "     - Disable: Allow merge commits"
  echo "     - Enable: Allow squash merging"
  echo "     - Disable: Allow rebase merging"
  echo "     - Enable: Always suggest updating pull request branches"
  echo "     - Enable: Automatically delete head branches"
  echo "     - Enable: Allow auto-merge"
  exit 1
fi
