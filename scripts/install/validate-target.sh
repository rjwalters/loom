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

FORGE_TYPE_DISPLAY=$(printf '%s' "$FORGE_TYPE" | awk '{print toupper(substr($0,1,1)) tolower(substr($0,2))}')
success "$FORGE_TYPE_DISPLAY repository: $REPO_NAME"

# Check git status - warn if dirty
if [[ -n "$(git status --porcelain)" ]]; then
  echo -e "${YELLOW}⚠ Warning: Working directory has uncommitted changes${NC}"
  echo -e "${YELLOW}  Installation will create a worktree, so this is safe to proceed.${NC}"
fi

# Target-state guard (#3327): warn/refuse if the target checkout is on a
# non-main branch or is behind origin/main. Dogfood exemption: when the
# target IS the Loom source repo (TARGET_PATH == LOOM_ROOT) the source-state
# check already covered this; don't double-warn.
#
# Environment inputs (exported by install-loom.sh parent):
#   ALLOW_STALE_TARGET=true|false  - operator override
#   NON_INTERACTIVE=true|false     - refuse vs. prompt
#   LOOM_ROOT                      - canonical Loom source path
ALLOW_STALE_TARGET="${ALLOW_STALE_TARGET:-false}"
NON_INTERACTIVE="${NON_INTERACTIVE:-false}"
if [[ "${LOOM_ROOT:-}" != "$TARGET_PATH" ]]; then
  target_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
  target_stale=""
  if git rev-parse --verify --quiet origin/main >/dev/null 2>&1; then
    target_behind=$(git rev-list --count HEAD..origin/main 2>/dev/null || echo 0)
    if [[ "$target_behind" -gt 0 ]]; then
      target_stale="local is $target_behind commit(s) behind origin/main (run 'git fetch && git pull' to refresh)"
    fi
  fi

  if [[ "$target_branch" != "main" ]] || [[ -n "$target_stale" ]]; then
    echo -e "${YELLOW}⚠ Warning: Target checkout is not on a clean main:${NC}"
    [[ "$target_branch" != "main" ]] && echo "    branch: $target_branch (expected: main)"
    [[ -n "$target_stale" ]]         && echo "    $target_stale"
    echo "  Target path: $TARGET_PATH"

    if [[ "$ALLOW_STALE_TARGET" == "true" ]]; then
      info "Continuing anyway (--allow-stale-target)"
    elif [[ "$NON_INTERACTIVE" == "true" ]]; then
      error "Refusing to install into non-main / stale target in --yes mode. Pass --allow-stale-target to override."
    else
      read -r -p "Proceed with this target checkout anyway? [y/N] " -n 1 reply
      echo ""
      if [[ ! "$reply" =~ ^[Yy]$ ]]; then
        error "Aborted by user. Switch the target to main and pull, or pass --allow-stale-target."
      fi
    fi
  fi
fi

success "Validation complete"
echo ""
echo "Target repository: $REPO_NAME"
echo "Target path: $TARGET_PATH"
echo "Forge type: $FORGE_TYPE"
