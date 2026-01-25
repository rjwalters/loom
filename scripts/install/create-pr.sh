#!/usr/bin/env bash
# Create pull request for Loom installation

set -euo pipefail

WORKTREE_PATH="${1:-.}"
BASE_BRANCH="${2:-}"
LOOM_VERSION="${LOOM_VERSION:-unknown}"
LOOM_COMMIT="${LOOM_COMMIT:-unknown}"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

error() {
  echo -e "${RED}✗ Error: $*${NC}" >&2
  exit 1
}

warning() {
  echo -e "${YELLOW}⚠ $*${NC}" >&2
}

info() {
  echo -e "${BLUE}ℹ $*${NC}" >&2
}

success() {
  echo -e "${GREEN}✓ $*${NC}" >&2
}

if [[ -z "$BASE_BRANCH" ]]; then
  error "Base branch required as second argument"
fi

cd "$WORKTREE_PATH"

# Detect the target repository from git remote
# This ensures we use the fork instead of upstream when both exist
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Could not determine repository from git remote"
fi

# Extract owner/repo from URL (handles both HTTPS and SSH)
# HTTPS: https://github.com/owner/repo.git -> owner/repo
# SSH: git@github.com:owner/repo.git -> owner/repo
REPO=$(echo "$ORIGIN_URL" | sed -E 's#^.*(github\.com[/:])##; s/\.git$//')

if [[ ! "$REPO" =~ ^[^/]+/[^/]+$ ]]; then
  error "Could not extract valid repository from URL: $ORIGIN_URL"
fi

info "Target repository: $REPO"

# Get current branch
BRANCH_NAME=$(git rev-parse --abbrev-ref HEAD)

info "Committing Loom installation..."

# Stage all changes
git add -A

# Check if there are changes to commit
if git diff --staged --quiet; then
  info "No changes to commit - Loom is already installed"
  info "Skipping commit and PR creation"
  # Exit successfully with a special marker that the caller can detect
  echo "NO_CHANGES_NEEDED"
  exit 0
fi

# Create commit message
COMMIT_MSG=$(cat <<EOF
Install Loom ${LOOM_VERSION} orchestration framework

Adds Loom configuration and GitHub workflow integration:
- .loom/ directory with configuration and scripts
- .claude/commands/ slash commands for roles (/builder, /judge, etc.)
- .claude/settings.json tool permissions
- .github/ labels and workflows
- Documentation (CLAUDE.md, AGENTS.md)

Loom Version: ${LOOM_VERSION}
Loom Commit: ${LOOM_COMMIT}
EOF
)

# Commit changes (redirect output to stderr so it doesn't interfere with PR URL capture)
git commit -m "$COMMIT_MSG" >&2
success "Changes committed"

info "Pushing branch: $BRANCH_NAME"

# Push branch (use --force in case branch exists remotely from previous failed installation)
# Redirect output to stderr so it doesn't interfere with PR URL capture
PUSH_OUTPUT=""
PUSH_EXIT_CODE=0
PUSH_OUTPUT=$(git push -u origin "$BRANCH_NAME" --force 2>&1) || PUSH_EXIT_CODE=$?

if [[ $PUSH_EXIT_CODE -ne 0 ]]; then
  # Check if the error is due to missing workflow scope
  if echo "$PUSH_OUTPUT" | grep -q "refusing to allow.*workflow"; then
    echo "" >&2
    echo -e "${YELLOW}⚠ GitHub rejected push: missing 'workflow' scope${NC}" >&2
    echo "" >&2
    info "The GitHub CLI token doesn't have permission to create workflow files."
    info "Retrying without workflow files..."
    echo "" >&2

    # Remove workflow files from the commit and retry
    WORKFLOW_FILES=$(git diff --name-only HEAD~1 HEAD 2>/dev/null | grep "^\.github/workflows/" || true)

    if [[ -n "$WORKFLOW_FILES" ]]; then
      # Unstage workflow files and amend the commit
      git reset HEAD~1 --soft >&2

      # Re-add everything except workflows
      git add -A >&2
      for wf in $WORKFLOW_FILES; do
        if [[ -f "$wf" ]]; then
          git reset HEAD -- "$wf" >&2 2>/dev/null || true
          rm -f "$wf"
        fi
      done

      # Amend commit message to note skipped workflows
      COMMIT_MSG_NO_WORKFLOW=$(cat <<EOF
Install Loom ${LOOM_VERSION} orchestration framework

Adds Loom configuration and GitHub workflow integration:
- .loom/ directory with configuration and scripts
- .claude/ MCP servers and prompts
- .github/ labels (workflows skipped - requires 'workflow' scope)
- Documentation (CLAUDE.md, AGENTS.md)

Note: GitHub workflow files were skipped due to missing 'workflow' scope.
To add workflows later, run: gh auth refresh -s workflow

Loom Version: ${LOOM_VERSION}
Loom Commit: ${LOOM_COMMIT}
EOF
)
      git commit -m "$COMMIT_MSG_NO_WORKFLOW" >&2

      # Retry push
      git push -u origin "$BRANCH_NAME" --force >&2 || {
        error "Failed to push even without workflow files"
      }

      echo "" >&2
      echo -e "${YELLOW}════════════════════════════════════════════════════════════${NC}" >&2
      echo -e "${YELLOW}  WORKFLOW FILES SKIPPED${NC}" >&2
      echo -e "${YELLOW}════════════════════════════════════════════════════════════${NC}" >&2
      echo "" >&2
      echo "  The following workflow files were NOT included:" >&2
      for wf in $WORKFLOW_FILES; do
        echo "    - $wf" >&2
      done
      echo "" >&2
      echo "  To add workflows later:" >&2
      echo "    1. Run: gh auth refresh -s workflow" >&2
      echo "    2. Manually copy workflows from Loom defaults:" >&2
      echo "       $LOOM_ROOT/defaults/.github/workflows/" >&2
      echo "" >&2
      echo -e "${YELLOW}════════════════════════════════════════════════════════════${NC}" >&2
      echo "" >&2

      success "Branch pushed (without workflows)"
    else
      # No workflow files found, but still failed - re-raise the error
      echo "$PUSH_OUTPUT" >&2
      error "Failed to push branch: $BRANCH_NAME"
    fi
  else
    # Different error - show output and fail
    echo "$PUSH_OUTPUT" >&2
    error "Failed to push branch: $BRANCH_NAME"
  fi
else
  echo "$PUSH_OUTPUT" >&2
  success "Branch pushed"
fi

info "Creating pull request..."

# Create PR body
PR_BODY=$(cat <<EOF
## Loom Installation

This PR adds Loom orchestration framework to the repository.

**Loom Version**: ${LOOM_VERSION}
**Loom Commit**: ${LOOM_COMMIT}

## What's Included

- ✅ \`.loom/\` - Configuration, roles, and scripts
- ✅ \`.claude/commands/\` - Slash commands for roles (/builder, /judge, /curator, etc.)
- ✅ \`.claude/settings.json\` - Claude Code tool permissions
- ✅ \`.github/\` - Labels and workflows
- ✅ \`CLAUDE.md\`/\`AGENTS.md\` - Documentation with Loom reference

## GitHub Labels

Synced Loom workflow labels via \`.github/labels.yml\`

## Next Steps

After merging:
1. Use \`/builder\`, \`/judge\`, etc. commands in Claude Code
2. Or install Loom.app for visual orchestration

See \`CLAUDE.md\` for complete usage details.

---
🤖 Generated by [Loom](https://github.com/rjwalters/loom) installation
EOF
)

# Create pull request and capture the URL
# Redirect stderr to stdout to capture the full output, then extract the URL
GH_PR_OUTPUT=$(gh pr create \
  -R "$REPO" \
  --base "$BASE_BRANCH" \
  --title "Install Loom ${LOOM_VERSION} (${LOOM_COMMIT})" \
  --body "$PR_BODY" \
  --label "loom:review-requested" 2>&1)

# Extract URL from output (gh CLI outputs the PR URL as the last line)
PR_URL=$(echo "$GH_PR_OUTPUT" | grep -oE 'https://github\.com/[^[:space:]]+/pull/[0-9]+' | head -1 | tr -d '[:space:]')

# Validate the URL
if [[ -z "$PR_URL" ]] || [[ ! "$PR_URL" =~ ^https://github\.com/[^[:space:]]+/pull/[0-9]+$ ]]; then
  echo "Error: Failed to create PR or invalid URL returned" >&2
  echo "gh output was:" >&2
  echo "$GH_PR_OUTPUT" >&2
  exit 1
fi

success "Pull request created: $PR_URL"

# Output the PR URL (stdout, so it can be captured by caller)
# Use exec to ensure we're writing directly to FD 1 without any buffering issues
exec 1>&1  # Ensure FD 1 is stdout
printf "%s" "$PR_URL"
