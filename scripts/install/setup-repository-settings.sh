#!/usr/bin/env bash
# Setup repository settings for Loom workflow
#
# Supports both GitHub and Gitea forges.
#
# Usage:
#   ./scripts/install/setup-repository-settings.sh /path/to/target-repo [--dry-run]
#
# Configures:
#   - Merge strategy: respects whatever the target repo already allows
#     (#7754) -- only defaults to enabling MERGE-COMMITS-ONLY when the repo
#     currently has EVERY merge strategy disabled (a degenerate state
#     neither forge's own UI normally permits; the #9105 default, since
#     merge commits preserve the full commit history Loom's blame/audit
#     tooling relies on). Before #7754 this unconditionally forced
#     squash-only (allow_merge_commit=false, allow_squash_merge=true,
#     allow_rebase_merge=false) on every installed repo, which broke
#     `merge-pr.sh`'s auto-merge on any repo an operator had deliberately
#     configured for merge-commit-only or rebase-only (the original #1258
#     bug: "Squash merges are not allowed on this repository").
#   - Auto-delete head branches
#   - Allow auto-merge
#   - Suggest updating branches (GitHub only; Gitea has no equivalent)

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

# --- GitHub repository settings ---
setup_github_repo_settings() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"

  # Check if user has admin permissions
  local has_admin
  has_admin=$(gh api "repos/${owner}/${repo}" --jq '.permissions.admin' 2>/dev/null || echo "false")
  if [[ "$has_admin" != "true" ]]; then
    warning "You may not have admin permissions to configure repository settings"
    warning "Attempting anyway (may fail with permission error)..."
  fi

  # Detect the repo's CURRENT merge-strategy settings (#7754) instead of
  # unconditionally forcing squash-only -- an operator who has deliberately
  # configured merge-commit-only or rebase-only must not have that silently
  # reverted by (re-)running the installer.
  local current_json current_squash current_merge current_rebase
  current_json=$(gh api "repos/${owner}/${repo}" 2>/dev/null || echo '{}')
  current_squash=$(echo "$current_json" | jq -r '.allow_squash_merge // false' 2>/dev/null || echo "false")
  current_merge=$(echo "$current_json" | jq -r '.allow_merge_commit // false' 2>/dev/null || echo "false")
  current_rebase=$(echo "$current_json" | jq -r '.allow_rebase_merge // false' 2>/dev/null || echo "false")

  local respect_existing=false
  if [[ "$current_squash" == "true" || "$current_merge" == "true" || "$current_rebase" == "true" ]]; then
    respect_existing=true
  fi

  # Build the settings payload: the merge-strategy trio (allow_merge_commit /
  # allow_squash_merge / allow_rebase_merge) is included ONLY when nothing is
  # currently enabled (a degenerate state GitHub's own UI does not normally
  # allow) -- in that fallback case we enable merge-commit-only (#9105) so
  # the repo can merge at all, preserving full commit history by default.
  # Otherwise we leave the trio out of the PATCH entirely so the repo's
  # existing choice is untouched.
  local settings_json
  if [[ "$respect_existing" == "true" ]]; then
    settings_json='{
      "delete_branch_on_merge": true,
      "allow_auto_merge": true,
      "allow_update_branch": true
    }'
  else
    settings_json='{
      "allow_merge_commit": true,
      "allow_squash_merge": false,
      "allow_rebase_merge": false,
      "delete_branch_on_merge": true,
      "allow_auto_merge": true,
      "allow_update_branch": true
    }'
  fi

  # In dry-run mode, just display what would be changed
  if [[ "$DRY_RUN" == "true" ]]; then
    echo ""
    info "DRY RUN - Would apply the following settings:"
    echo ""
    echo "  Repository: ${owner}/${repo}"
    echo "  Forge: GitHub"
    echo ""
    if [[ "$respect_existing" == "true" ]]; then
      echo "  Current merge-strategy settings (unchanged):"
      echo "    allow_merge_commit: ${current_merge}"
      echo "    allow_squash_merge: ${current_squash}"
      echo "    allow_rebase_merge: ${current_rebase}"
      echo ""
      info "Repository already allows at least one merge strategy -- respecting existing configuration"
    else
      echo "  No merge strategy is currently enabled -- would apply a fallback default:"
      echo "    allow_merge_commit: true (default strategy - preserves full commit history)"
      echo "    allow_squash_merge: false (disabled)"
      echo "    allow_rebase_merge: false (disabled)"
    fi
    echo ""
    echo "  Settings to be configured:"
    echo "    delete_branch_on_merge: true (auto-cleanup branches)"
    echo "    allow_auto_merge: true (enables Champion auto-merge)"
    echo "    allow_update_branch: true (suggest branch updates)"
    echo ""
    info "No changes made (dry-run mode)"
    return 0
  fi

  # Apply repository settings using GitHub API
  if echo "$settings_json" | gh api "repos/${owner}/${repo}" -X PATCH --input - > /dev/null 2>&1; then
    success "Repository settings configured successfully"
    echo ""
    echo "Applied settings:"
    if [[ "$respect_existing" == "true" ]]; then
      echo "  - Merge strategy: unchanged (repo already allows: merge=${current_merge}, squash=${current_squash}, rebase=${current_rebase})"
    else
      echo "  - Allow merge commits: Yes (default strategy - preserves full commit history)"
      echo "  - Allow squash merging: No (disabled)"
      echo "  - Allow rebase merging: No (disabled)"
    fi
    echo "  - Delete branches on merge: Yes (auto-cleanup)"
    echo "  - Allow auto-merge: Yes (enables Champion workflow)"
    echo "  - Suggest updating branches: Yes"
    echo ""
    info "To modify: GitHub Settings > General > Pull Requests"
    return 0
  else
    error "Failed to configure repository settings"
    echo ""
    echo "This can happen if:"
    echo "  - You lack admin permissions on ${owner}/${repo}"
    echo "  - GitHub API is unreachable"
    echo ""
    info "To configure manually:"
    echo "  1. Go to: https://github.com/${owner}/${repo}/settings"
    echo "  2. Scroll to 'Pull Requests' section"
    echo "  3. Configure merge options:"
    echo "     - Enable at least one of: Allow merge commits / Allow squash merging / Allow rebase merging"
    echo "     - Enable: Always suggest updating pull request branches"
    echo "     - Enable: Automatically delete head branches"
    echo "     - Enable: Allow auto-merge"
    return 1
  fi
}

# --- Gitea repository settings ---
setup_gitea_repo_settings() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"

  if [[ -z "$FORGE_TOKEN" ]]; then
    error "Gitea API token required. Set GITEA_TOKEN or FORGE_TOKEN environment variable."
    return 1
  fi

  # Detect the repo's CURRENT merge-strategy settings (#7754) instead of
  # unconditionally forcing squash-only.
  local current_json current_squash current_merge current_rebase current_style
  current_json=$(gitea_api GET "/repos/${owner}/${repo}" 2>/dev/null | sed '$d')
  current_squash=$(echo "$current_json" | jq -r '.allow_squash_merge // false' 2>/dev/null || echo "false")
  current_merge=$(echo "$current_json" | jq -r '.allow_merge_commits // false' 2>/dev/null || echo "false")
  current_rebase=$(echo "$current_json" | jq -r '.allow_rebase_merge // false' 2>/dev/null || echo "false")
  current_style=$(echo "$current_json" | jq -r '.default_merge_style // empty' 2>/dev/null || echo "")

  local respect_existing=false
  if [[ "$current_squash" == "true" || "$current_merge" == "true" || "$current_rebase" == "true" ]]; then
    respect_existing=true
  fi

  # Gitea repo settings payload: the merge-strategy trio + default_merge_style
  # is included ONLY when nothing is currently enabled (fallback: enable
  # merge-commit-only so the repo can merge at all, #9105). Otherwise the
  # trio and merge style are left out of the PATCH so an operator's existing
  # choice (e.g. squash-only or rebase-only, the original #1258 bug class)
  # is untouched.
  local settings_json
  if [[ "$respect_existing" == "true" ]]; then
    settings_json='{
      "default_delete_branch_after_merge": true
    }'
  else
    settings_json='{
      "allow_merge_commits": true,
      "allow_squash_merge": false,
      "allow_rebase_merge": false,
      "default_delete_branch_after_merge": true,
      "default_merge_style": "merge"
    }'
  fi

  # In dry-run mode, just display what would be changed
  if [[ "$DRY_RUN" == "true" ]]; then
    echo ""
    info "DRY RUN - Would apply the following settings:"
    echo ""
    echo "  Repository: ${owner}/${repo}"
    echo "  Forge: Gitea"
    echo ""
    if [[ "$respect_existing" == "true" ]]; then
      echo "  Current merge-strategy settings (unchanged):"
      echo "    allow_merge_commits: ${current_merge}"
      echo "    allow_squash_merge: ${current_squash}"
      echo "    allow_rebase_merge: ${current_rebase}"
      echo "    default_merge_style: ${current_style:-unset}"
      echo ""
      info "Repository already allows at least one merge strategy -- respecting existing configuration"
    else
      echo "  No merge strategy is currently enabled -- would apply a fallback default:"
      echo "    allow_merge_commits: true (default strategy - preserves full commit history)"
      echo "    allow_squash_merge: false (disabled)"
      echo "    allow_rebase_merge: false (disabled)"
      echo "    default_merge_style: merge"
    fi
    echo ""
    echo "  Settings to be configured:"
    echo "    default_delete_branch_after_merge: true (auto-cleanup branches)"
    echo ""
    warning "Gitea does not support 'allow_update_branch' (suggest branch updates)."
    echo "  This is a cosmetic GitHub UI feature with low impact."
    echo ""
    warning "Gitea has limited auto-merge support via 'allow_merge_by_api'."
    echo "  Loom's Champion role uses explicit merge API calls, so this is handled."
    echo ""
    info "No changes made (dry-run mode)"
    return 0
  fi

  # Apply repository settings using Gitea API
  local response http_code
  response=$(gitea_api PATCH "/repos/${owner}/${repo}" "$settings_json")
  http_code=$(echo "$response" | tail -1)

  if [[ "$http_code" == "200" ]]; then
    success "Repository settings configured successfully"
    echo ""
    echo "Applied settings:"
    if [[ "$respect_existing" == "true" ]]; then
      echo "  - Merge strategy: unchanged (repo already allows: merge=${current_merge}, squash=${current_squash}, rebase=${current_rebase})"
    else
      echo "  - Allow merge commits: Yes (default strategy - preserves full commit history)"
      echo "  - Allow squash merging: No (disabled)"
      echo "  - Allow rebase merging: No (disabled)"
      echo "  - Default merge style: merge (enforces merge-commit-only)"
    fi
    echo "  - Delete branches on merge: Yes (auto-cleanup)"
    echo ""

    # Graceful degradation warnings
    warning "Gitea does not support 'allow_update_branch' (suggest branch updates)."
    echo "  This is a cosmetic GitHub UI feature with low impact."
    echo ""
    warning "Gitea has limited auto-merge support."
    echo "  Loom's Champion role uses explicit merge API calls, so this is handled."
    echo ""

    info "To modify: Gitea Settings > Repository"
    return 0
  else
    error "Failed to configure repository settings (HTTP ${http_code})"
    echo ""
    echo "This can happen if:"
    echo "  - You lack admin permissions on ${owner}/${repo}"
    echo "  - Gitea API is unreachable"
    echo "  - The auth token is invalid or expired"
    echo ""
    info "To configure manually:"
    echo "  1. Go to your Gitea repository settings"
    echo "  2. Under 'Repository', configure merge options:"
    echo "     - Enable at least one of: Allow merge commits / Allow squash merging / Allow rebase merging"
    echo "     - Enable: Automatically delete head branches"
    echo "     - Set default merge style to match your enabled strategy"
    return 1
  fi
}

# ============================================================================
# Main
# ============================================================================

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
info "Configuring repository settings for: ${FORGE_OWNER}/${FORGE_REPO}"

# Dispatch to the appropriate forge handler
if [[ "$FORGE_TYPE" == "github" ]]; then
  setup_github_repo_settings
elif [[ "$FORGE_TYPE" == "gitea" ]]; then
  setup_gitea_repo_settings
else
  error "Unsupported forge type: ${FORGE_TYPE}"
  exit 1
fi
