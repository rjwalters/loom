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

# Create issue and capture the number
ISSUE_NUMBER=$(gh issue create \
  --title "Install Loom ${LOOM_VERSION}" \
  --body "$ISSUE_BODY" \
  --label "loom:in-progress" \
  --json number --jq '.number')

success "Created issue #${ISSUE_NUMBER}"

# Output the issue number (stdout, so it can be captured by caller)
echo "$ISSUE_NUMBER"
