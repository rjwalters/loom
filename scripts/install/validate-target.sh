#!/usr/bin/env bash
# Validate target repository for Loom installation

set -euo pipefail

TARGET_PATH="${1:-.}"

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

# Check if we can determine the remote repository
cd "$TARGET_PATH"

# Detect the repository from origin remote (not upstream)
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Unable to determine GitHub repository. Ensure origin remote is set."
fi

# Extract owner/repo from URL (handles both HTTPS and SSH)
# HTTPS: https://github.com/owner/repo.git -> owner/repo
# SSH: git@github.com:owner/repo.git -> owner/repo
REPO_NAME=$(echo "$ORIGIN_URL" | sed -E 's#^.*(github\.com[/:])##; s/\.git$//')

if [[ ! "$REPO_NAME" =~ ^[^/]+/[^/]+$ ]]; then
  error "Could not extract valid repository from URL: $ORIGIN_URL"
fi

# Verify gh can access this repository
if ! gh repo view "$REPO_NAME" &> /dev/null; then
  error "Unable to access GitHub repository: $REPO_NAME. Check your gh authentication."
fi

success "GitHub repository: $REPO_NAME"

# Check git status - warn if dirty
if [[ -n "$(git status --porcelain)" ]]; then
  echo -e "${YELLOW}⚠ Warning: Working directory has uncommitted changes${NC}"
  echo -e "${YELLOW}  Installation will create a worktree, so this is safe to proceed.${NC}"
fi

success "Validation complete"
echo ""
echo "Target repository: $REPO_NAME"
echo "Target path: $TARGET_PATH"
