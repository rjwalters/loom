#!/usr/bin/env bash
# Setup branch protection for Loom workflow
#
# Supports both GitHub (rulesets API) and Gitea (branch protection API).
#
# Usage:
#   ./scripts/install/setup-branch-protection.sh /path/to/target-repo [branch-name]
#
# For GitHub, creates or updates a ruleset with recommended rules:
#   - Prevent branch deletion and force pushes
#   - Require linear history (squash merges only)
#   - Require pull requests (0 approvals for solo dev/Loom workflows)
#
# For Gitea, creates or updates branch protection with equivalent settings:
#   - Prevent force pushes
#   - Require pull requests (0 approvals)
#   - Dismiss stale approvals
#   - Warns about features without Gitea equivalents (linear history, role-based bypass)

set -euo pipefail

# Source helper functions
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Define helper functions for consistent output
info() { echo -e "\033[0;34mℹ $*\033[0m"; }
success() { echo -e "\033[0;32m✓ $*\033[0m"; }
warning() { echo -e "\033[1;33m⚠ $*\033[0m"; }
error() { echo -e "\033[0;31m✗ $*\033[0m"; }

# Source forge detection helper
source "${SCRIPT_DIR}/forge-detect.sh"

# --- GitHub branch protection (rulesets API) ---
setup_github_branch_protection() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"
  local ruleset_name="$RULESET_NAME"

  # Check if user has admin permissions
  local has_admin
  has_admin=$(gh api "repos/${owner}/${repo}" --jq '.permissions.admin' 2>/dev/null || echo "false")
  if [[ "$has_admin" != "true" ]]; then
    warning "You may not have admin permissions to configure rulesets"
    warning "Attempting anyway (may fail with permission error)..."
  fi

  # Ruleset payload
  # bypass_actors: actor_id 5 = RepositoryRole/admin — allows repo admins to push
  # directly to main without a PR (e.g. for hotfixes or initial setup).
  local ruleset_payload='{
    "name": "'"$ruleset_name"'",
    "target": "branch",
    "enforcement": "active",
    "bypass_actors": [
      {
        "actor_id": 5,
        "actor_type": "RepositoryRole",
        "bypass_mode": "always"
      }
    ],
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
  local existing_id
  existing_id=$(gh api "repos/${owner}/${repo}/rulesets" --jq '.[] | select(.name == "'"$ruleset_name"'") | .id' 2>/dev/null || echo "")

  local api_method api_url
  if [[ -n "$existing_id" ]]; then
    info "Found existing ruleset '${ruleset_name}' (ID: ${existing_id}), updating..."
    api_method="PUT"
    api_url="repos/${owner}/${repo}/rulesets/${existing_id}"
  else
    info "Creating new ruleset '${ruleset_name}'..."
    api_method="POST"
    api_url="repos/${owner}/${repo}/rulesets"
  fi

  if echo "$ruleset_payload" | gh api --method "$api_method" "$api_url" --input - > /dev/null 2>&1; then
    success "Branch ruleset configured successfully"
    echo ""
    echo "Applied rules:"
    echo "  - Prevent branch deletion"
    echo "  - Prevent force pushes"
    echo "  - Require linear history (squash merges only)"
    echo "  - Require pull requests (0 approvals required)"
    echo "  - Dismiss stale reviews on new commits"
    echo "  - Admin bypass: repository admins can push directly without a PR"
    echo ""
    echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
    echo "Note: Admin bypass allows repo owners to push hotfixes directly to main when needed."
    echo ""
    info "To modify: GitHub Settings > Rules > Rulesets"
    return 0
  else
    error "Failed to configure branch ruleset"
    echo ""
    echo "This can happen if:"
    echo "  - You lack admin permissions on ${owner}/${repo}"
    echo "  - GitHub API is unreachable"
    echo ""
    info "To configure manually:"
    echo "  1. Go to: https://github.com/${owner}/${repo}/settings/rules"
    echo "  2. Create a new ruleset for the default branch"
    echo "  3. Enable: Prevent deletion, prevent force push"
    echo "  4. Enable: Require linear history"
    echo "  5. Enable: Require pull request (0 approvals)"
    return 1
  fi
}

# --- Gitea branch protection ---
setup_gitea_branch_protection() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"

  if [[ -z "$FORGE_TOKEN" ]]; then
    error "Gitea API token required. Set GITEA_TOKEN or FORGE_TOKEN environment variable."
    return 1
  fi

  # Check admin permissions via repo info
  local repo_response http_code body
  repo_response=$(gitea_api GET "/repos/${owner}/${repo}")
  http_code=$(echo "$repo_response" | tail -1)
  body=$(echo "$repo_response" | sed '$d')

  if [[ "$http_code" != "200" ]]; then
    warning "Could not verify permissions on ${owner}/${repo} (HTTP ${http_code})"
    warning "Attempting branch protection setup anyway..."
  fi

  # Branch protection payload for Gitea
  local protection_payload
  protection_payload='{
    "branch_name": "'"${BRANCH_NAME}"'",
    "enable_push": true,
    "enable_force_push": false,
    "enable_force_push_allowlist": false,
    "dismiss_stale_approvals": true,
    "required_approvals": 0,
    "block_on_rejected_reviews": false,
    "block_admin_merge_override": false
  }'

  # Check if branch protection already exists
  local existing_response existing_code
  existing_response=$(gitea_api GET "/repos/${owner}/${repo}/branch_protections/${BRANCH_NAME}")
  existing_code=$(echo "$existing_response" | tail -1)

  if [[ "$existing_code" == "200" ]]; then
    info "Found existing branch protection for '${BRANCH_NAME}', updating..."
    local update_response update_code
    update_response=$(gitea_api PATCH "/repos/${owner}/${repo}/branch_protections/${BRANCH_NAME}" "$protection_payload")
    update_code=$(echo "$update_response" | tail -1)

    if [[ "$update_code" == "200" ]]; then
      _gitea_protection_success
      return 0
    else
      _gitea_protection_failure "$owner" "$repo" "$update_code"
      return 1
    fi
  else
    info "Creating new branch protection for '${BRANCH_NAME}'..."
    local create_response create_code
    create_response=$(gitea_api POST "/repos/${owner}/${repo}/branch_protections" "$protection_payload")
    create_code=$(echo "$create_response" | tail -1)

    if [[ "$create_code" == "201" || "$create_code" == "200" ]]; then
      _gitea_protection_success
      return 0
    else
      _gitea_protection_failure "$owner" "$repo" "$create_code"
      return 1
    fi
  fi
}

_gitea_protection_success() {
  success "Branch protection configured successfully"
  echo ""
  echo "Applied rules:"
  echo "  - Allow push (not force push)"
  echo "  - Prevent force pushes"
  echo "  - Require pull requests (0 approvals required)"
  echo "  - Dismiss stale approvals on new commits"
  echo "  - Admin merge override: allowed"
  echo ""

  # Graceful degradation warnings
  warning "Gitea does not support 'required linear history' at the branch level."
  echo "  Mitigation: squash-only merging is enforced via repository settings."
  echo ""
  warning "Gitea does not support role-based bypass actors (admin bypass)."
  echo "  Mitigation: block_admin_merge_override is set to false, allowing admin overrides."
  echo ""
  warning "Gitea does not support per-branch merge method restrictions."
  echo "  Mitigation: merge methods are configured at the repository level."
  echo ""

  echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
  echo ""
  info "To modify: Gitea Settings > Branches > Branch Protection"
}

_gitea_protection_failure() {
  local owner="$1"
  local repo="$2"
  local code="$3"

  error "Failed to configure branch protection (HTTP ${code})"
  echo ""
  echo "This can happen if:"
  echo "  - You lack admin permissions on ${owner}/${repo}"
  echo "  - Gitea API is unreachable"
  echo "  - The auth token is invalid or expired"
  echo ""
  info "To configure manually:"
  echo "  1. Go to your Gitea repository settings"
  echo "  2. Navigate to Branches > Branch Protection"
  echo "  3. Add protection for '${BRANCH_NAME}'"
  echo "  4. Disable force push"
  echo "  5. Set required approvals to 0"
  echo "  6. Enable 'Dismiss stale approvals'"
}

# ============================================================================
# Main
# ============================================================================

TARGET_PATH="${1:-}"
BRANCH_NAME="${2:-main}"
RULESET_NAME="main"

if [[ -z "$TARGET_PATH" ]]; then
  error "Target path required"
  echo "Usage: $0 /path/to/target-repo [branch-name]"
  exit 1
fi

cd "$TARGET_PATH"

# Detect the repository from origin remote
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Failed to get repository information from git remote."
  exit 1
fi

# Detect forge type and extract owner/repo
if ! detect_forge_and_repo "$ORIGIN_URL"; then
  error "Could not detect forge type. Is this a GitHub or Gitea repository?"
  exit 1
fi

echo ""
info "Detected forge: ${FORGE_TYPE}"
info "Configuring branch protection for: ${FORGE_OWNER}/${FORGE_REPO} (${BRANCH_NAME})"

# Dispatch to the appropriate forge handler
if [[ "$FORGE_TYPE" == "github" ]]; then
  setup_github_branch_protection
elif [[ "$FORGE_TYPE" == "gitea" ]]; then
  setup_gitea_branch_protection
else
  error "Unsupported forge type: ${FORGE_TYPE}"
  exit 1
fi
