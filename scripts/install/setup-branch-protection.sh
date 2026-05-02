#!/usr/bin/env bash
# Setup branch protection for Loom workflow
#
# Supports both GitHub (rulesets API) and Gitea (branch protection API).
#
# Usage:
#   ./scripts/install/setup-branch-protection.sh /path/to/target-repo [branch-name]
#
# Environment:
#   LOOM_NON_INTERACTIVE=true  Skip prompts; on overlap conflict, default to
#                              "skip" (preserves existing protection, avoids
#                              creating duplicate rulesets — issue #3216).
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

# Enumerate rulesets that target the default branch via cross-name overlap.
# Sets array OVERLAPPING_RULESETS to "id|name|enforcement" lines for any
# active/evaluate ruleset whose conditions include ~DEFAULT_BRANCH,
# refs/heads/<branch>, or refs/heads/*, AND whose name does not match
# RULESET_NAME (the same-named case is handled by the in-place update path).
# Disabled rulesets are ignored (they cannot conflict at runtime).
detect_overlapping_rulesets() {
  local owner="$1"
  local repo="$2"
  local our_name="$3"
  local branch="$4"

  OVERLAPPING_RULESETS=()

  local rulesets_json
  rulesets_json=$(gh api "repos/${owner}/${repo}/rulesets" 2>/dev/null || echo "[]")

  while IFS='|' read -r rs_id rs_name rs_enforcement; do
    [[ -z "$rs_id" ]] && continue
    # Skip our own same-named ruleset; handled by existing in-place update path
    if [[ "$rs_name" == "$our_name" ]]; then
      continue
    fi
    # Ignore disabled rulesets (inactive, can't conflict at runtime)
    if [[ "$rs_enforcement" == "disabled" ]]; then
      continue
    fi

    # Fetch detail to inspect ref_name conditions
    local detail
    detail=$(gh api "repos/${owner}/${repo}/rulesets/${rs_id}" 2>/dev/null || echo "{}")

    local includes
    includes=$(echo "$detail" | jq -r '.conditions.ref_name.include // [] | .[]' 2>/dev/null || echo "")

    # Match ~DEFAULT_BRANCH token, refs/heads/<branch>, or wildcard refs/heads/*
    if echo "$includes" | grep -qE "^(~DEFAULT_BRANCH|refs/heads/${branch}|refs/heads/\*)$"; then
      OVERLAPPING_RULESETS+=("${rs_id}|${rs_name}|${rs_enforcement}")
    fi
  done < <(echo "$rulesets_json" | jq -r '.[] | select(.target == "branch") | "\(.id)|\(.name)|\(.enforcement)"' 2>/dev/null || true)
}

setup_github_branch_protection() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"
  local ruleset_name="$RULESET_NAME"
  local branch="$BRANCH_NAME"

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

  # Detect cross-name overlapping rulesets BEFORE the same-name update path,
  # so we don't silently POST a second ruleset overlapping a differently-named
  # pre-existing one. See issue #3216 for the bug this fixes.
  detect_overlapping_rulesets "$owner" "$repo" "$ruleset_name" "$branch"

  if (( ${#OVERLAPPING_RULESETS[@]} > 0 )); then
    warning "Found ${#OVERLAPPING_RULESETS[@]} existing ruleset(s) targeting the default branch:"
    local entry rs_id rs_name rs_enforcement
    for entry in "${OVERLAPPING_RULESETS[@]}"; do
      IFS='|' read -r rs_id rs_name rs_enforcement <<< "$entry"
      echo "    - id=${rs_id} name='${rs_name}' enforcement=${rs_enforcement}"
    done
    echo ""

    # Determine action: in non-interactive mode, default to Skip (safest).
    local action="skip"
    if [[ "${LOOM_NON_INTERACTIVE:-false}" != "true" ]]; then
      echo "How would you like to handle this?"
      echo "  [s] Skip    - keep existing ruleset(s), do not add Loom's (default, safest)"
      echo "  [r] Replace - delete the conflicting ruleset(s), then add Loom's"
      echo "  [u] Update  - update the first conflicting ruleset in-place with Loom's rules"
      echo ""
      local reply
      read -p "Choose [s/r/u] (default: s): " -n 1 -r reply
      echo ""
      case "$reply" in
        r|R) action="replace" ;;
        u|U) action="update" ;;
        *)   action="skip" ;;
      esac
    else
      info "Non-interactive mode: defaulting to 'skip' to avoid creating duplicate rulesets"
    fi

    case "$action" in
      skip)
        info "Skipping ruleset creation; existing protection is preserved."
        info "To replace later, re-run interactively or delete the existing ruleset first."
        return 0
        ;;
      replace)
        info "Replacing conflicting ruleset(s)..."
        for entry in "${OVERLAPPING_RULESETS[@]}"; do
          IFS='|' read -r rs_id rs_name rs_enforcement <<< "$entry"
          info "  Deleting ruleset id=${rs_id} name='${rs_name}'"
          if ! gh api --method DELETE "repos/${owner}/${repo}/rulesets/${rs_id}" > /dev/null 2>&1; then
            error "Failed to delete ruleset id=${rs_id}; aborting to avoid duplicates."
            return 1
          fi
        done
        # Fall through to standard create/update path below.
        ;;
      update)
        # Update the first conflicting ruleset in place with Loom's rules.
        # Preserves the existing ruleset's name/id; PUTs the rules onto it.
        IFS='|' read -r rs_id rs_name rs_enforcement <<< "${OVERLAPPING_RULESETS[0]}"
        info "Updating ruleset id=${rs_id} name='${rs_name}' in place with Loom rules..."
        local update_payload
        update_payload=$(echo "$ruleset_payload" | jq --arg n "$rs_name" '.name = $n')
        if echo "$update_payload" | gh api --method PUT "repos/${owner}/${repo}/rulesets/${rs_id}" --input - > /dev/null 2>&1; then
          success "Branch ruleset updated in place (id=${rs_id} name='${rs_name}')"
          echo ""
          info "To modify: GitHub Settings > Rules > Rulesets"
          return 0
        else
          error "Failed to update existing ruleset id=${rs_id}"
          return 1
        fi
        ;;
    esac
  fi

  # Check if a ruleset named "main" already exists (same-name in-place update).
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

  # Check if branch protection already exists.
  # Gitea's API is keyed by branch name (unlike GitHub's rulesets, which are
  # keyed by id and can have many overlapping rulesets per branch). So this
  # upsert by branch name fully covers conflict detection — no analogue of
  # the cross-name overlap bug from issue #3216 exists here.
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
