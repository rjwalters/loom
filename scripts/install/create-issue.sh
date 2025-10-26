#!/usr/bin/env bash
# Create GitHub issue for Loom installation tracking

set -euo pipefail

TARGET_PATH="${1:-.}"
LOOM_VERSION="${LOOM_VERSION:-unknown}"
LOOM_COMMIT="${LOOM_COMMIT:-unknown}"

# ANSI color codes
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

info() {
  echo -e "${BLUE}ℹ $*${NC}" >&2
}

success() {
  echo -e "${GREEN}✓ $*${NC}" >&2
}

cd "$TARGET_PATH"

# Detect the target repository from git remote
# This ensures we use the fork instead of upstream when both exist
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  echo "Error: Could not determine repository from git remote" >&2
  exit 1
fi

# Extract owner/repo from URL (handles both HTTPS and SSH)
# HTTPS: https://github.com/owner/repo.git -> owner/repo
# SSH: git@github.com:owner/repo.git -> owner/repo
REPO=$(echo "$ORIGIN_URL" | sed -E 's#^.*(github\.com[/:])##; s/\.git$//')

if [[ ! "$REPO" =~ ^[^/]+/[^/]+$ ]]; then
  echo "Error: Could not extract valid repository from URL: $ORIGIN_URL" >&2
  exit 1
fi

info "Target repository: $REPO"

# Get current date
INSTALL_DATE=$(date +%Y-%m-%d)

# Create issue body
ISSUE_BODY=$(cat <<EOF
Install Loom orchestration framework.

**Loom Version**: ${LOOM_VERSION}
**Loom Commit**: ${LOOM_COMMIT}
**Installation Date**: ${INSTALL_DATE}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode with Claude Code terminals).

## Installation includes:

- [ ] \`.loom/\` configuration directory
- [ ] \`.claude/\` MCP servers and prompts
- [ ] \`.github/\` labels and workflows
- [ ] \`CLAUDE.md\` AI context documentation
- [ ] \`AGENTS.md\` Agent workflow guide

## Repository

https://github.com/loomhq/loom
EOF
)

info "Creating installation issue..."

# Create issue and capture the URL
# NOTE: Don't add label during creation - labels are synced in Step 5
ISSUE_URL=$(gh issue create \
  -R "$REPO" \
  --title "Install Loom ${LOOM_VERSION} (${LOOM_COMMIT})" \
  --body "$ISSUE_BODY" 2>&1 | grep -oE 'https://github\.com/[^[:space:]]+/issues/[0-9]+' | head -1 | tr -d '\n\r')

# Validate URL was created
if [[ -z "$ISSUE_URL" ]]; then
  echo "Error: Failed to create issue or extract issue URL" >&2
  exit 1
fi

# Extract issue number from URL
ISSUE_NUMBER=$(echo "$ISSUE_URL" | grep -oE '[0-9]+$' | head -1 | tr -d '\n\r')

# Validate issue number
if [[ -z "$ISSUE_NUMBER" ]] || [[ ! "$ISSUE_NUMBER" =~ ^[0-9]+$ ]]; then
  echo "Error: Failed to extract valid issue number from URL: $ISSUE_URL" >&2
  exit 1
fi

success "Created issue #${ISSUE_NUMBER}"

# Try to add loom:building label if it exists (graceful failure if label doesn't exist yet)
info "Adding loom:building label..."
if gh issue edit "$ISSUE_NUMBER" -R "$REPO" --add-label "loom:building" >/dev/null 2>&1; then
  success "Added loom:building label"
else
  info "Label will be added after label sync (Step 5)"
fi

# Output the issue number (stdout, so it can be captured by caller)
printf "%s" "$ISSUE_NUMBER"
