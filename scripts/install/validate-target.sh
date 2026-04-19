#!/usr/bin/env bash
# Validate target repository for Loom installation
#
# Supports both GitHub and Gitea repositories.
# For GitHub: requires gh CLI and authentication.
# For Gitea: requires GITEA_TOKEN or FORGE_TOKEN environment variable.

set -euo pipefail

TARGET_PATH="${1:-.}"

# Source forge detection helper
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/forge-detect.sh"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

error() {
  echo -e "${RED}✗ Error: $*${NC}" >&2
  exit 1
}

info() {
  echo -e "${BLUE}ℹ $*${NC}"
}

success() {
  echo -e "${GREEN}✓ $*${NC}"
}

# Resolve to absolute path
TARGET_PATH="$(cd "$TARGET_PATH" && pwd)" || error "Target path does not exist: $1"

info "Validating target: $TARGET_PATH"

# Check if target is a git repository
if [[ ! -d "$TARGET_PATH/.git" ]]; then
  error "Target is not a git repository: $TARGET_PATH"
fi
success "Git repository detected"

# Check if we can determine the remote repository
cd "$TARGET_PATH"

# Detect the repository from origin remote (not upstream)
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Unable to determine remote repository. Ensure origin remote is set."
fi

# Detect forge type and extract owner/repo
if ! detect_forge_and_repo "$ORIGIN_URL"; then
  error "Could not detect forge type from URL: $ORIGIN_URL"
fi

REPO_NAME="${FORGE_OWNER}/${FORGE_REPO}"

# Forge-specific validation
if [[ "$FORGE_TYPE" == "github" ]]; then
  # Check if gh CLI is available
  if ! command -v gh &> /dev/null; then
    error "GitHub CLI (gh) is not installed. Install from: https://cli.github.com/"
  fi
  success "GitHub CLI (gh) available"

  # Check if gh is authenticated
  if ! gh auth status &> /dev/null; then
    error "GitHub CLI is not authenticated. Run: gh auth login"
  fi
  success "GitHub CLI authenticated"

  # Verify gh can access this repository
  if ! gh repo view "$REPO_NAME" &> /dev/null; then
    error "Unable to access repository: $REPO_NAME. Check your gh authentication."
  fi

elif [[ "$FORGE_TYPE" == "gitea" ]]; then
  # Validate Gitea token
  if [[ -z "$FORGE_TOKEN" ]]; then
    error "Gitea API token required. Set GITEA_TOKEN or FORGE_TOKEN environment variable.\n       Create a token at: <your-gitea-instance>/user/settings/applications"
  fi
  success "Gitea API token configured"

  # Verify API access to the repository
  local_response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}")
  local_code=$(echo "$local_response" | tail -1)
  if [[ "$local_code" != "200" ]]; then
    error "Unable to access Gitea repository: $REPO_NAME (HTTP $local_code). Check your API token and URL."
  fi
  success "Gitea API access verified"
fi

success "${FORGE_TYPE^} repository: $REPO_NAME"

# Check git status - warn if dirty
if [[ -n "$(git status --porcelain)" ]]; then
  echo -e "${YELLOW}⚠ Warning: Working directory has uncommitted changes${NC}"
  echo -e "${YELLOW}  Installation will create a worktree, so this is safe to proceed.${NC}"
fi

success "Validation complete"
echo ""
echo "Target repository: $REPO_NAME"
echo "Target path: $TARGET_PATH"
echo "Forge type: $FORGE_TYPE"
