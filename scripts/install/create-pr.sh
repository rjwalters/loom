#!/usr/bin/env bash
# Create pull request for Loom installation or uninstallation
#
# Supports both GitHub (via gh CLI) and Gitea (via API).
#
# Default behavior (issue #3333):
#   The auto-generated install PR carries three passive "docs-only" markers so
#   target repositories that opt-in via path-ignore, title-prefix matching, or
#   commit-message scanning can skip expensive CI for install PRs:
#
#     1. PR title prefix: "chore(loom): Install Loom <version>"
#     2. PR body marker line (separator-isolated): "loom-install: true"
#     3. Commit message trailer:                   "Skip-CI-Hint: docs-only"
#
#   These markers are PASSIVE — they do not include `[skip ci]` or `[ci skip]`
#   directives and therefore do not suppress CI globally. Target repos must
#   opt-in explicitly (path-ignore, custom workflow filter) for them to take
#   effect.
#
#   Set SKIP_TARGET_CI=true (or pass `--skip-target-ci` to install-loom.sh)
#   to additionally prepend `[skip ci]` to the PR title and commit subject —
#   the universal GitHub-native CI skip directive.
#
# Environment variables:
#   LOOM_VERSION       - Loom version string (default: "unknown")
#   LOOM_COMMIT        - Loom commit hash (default: "unknown")
#   FORCE_AUTO_MERGE   - If "true", attempt to auto-merge the PR (default: "false")
#   SKIP_TARGET_CI     - If "true", add `[skip ci]` prefix to PR title and
#                        commit subject (default: "false"). Opt-in via the
#                        `--skip-target-ci` flag on install-loom.sh.
#   PR_TITLE           - Custom PR title (default: auto-generated install title
#                        with `chore(loom): ` prefix). When set, FULLY OVERRIDES
#                        the default — no marker injection occurs.
#   PR_BODY            - Custom PR body (default: auto-generated install body
#                        with `loom-install: true` marker line). When set,
#                        FULLY OVERRIDES the default — no marker injection.
#   COMMIT_MSG         - Custom commit message (default: auto-generated install
#                        message with `Skip-CI-Hint: docs-only` trailer). When
#                        set, FULLY OVERRIDES the default — no marker injection.

set -euo pipefail

WORKTREE_PATH="${1:-.}"
BASE_BRANCH="${2:-}"
LOOM_VERSION="${LOOM_VERSION:-unknown}"
LOOM_COMMIT="${LOOM_COMMIT:-unknown}"
FORCE_AUTO_MERGE="${FORCE_AUTO_MERGE:-false}"
SKIP_TARGET_CI="${SKIP_TARGET_CI:-false}"

# Source forge detection helper
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/forge-detect.sh"

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
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Could not determine repository from git remote"
fi

# Detect forge type and extract owner/repo
if ! detect_forge_and_repo "$ORIGIN_URL"; then
  error "Could not detect forge type from URL: $ORIGIN_URL"
fi

REPO="${FORGE_OWNER}/${FORGE_REPO}"
info "Target repository: $REPO (${FORGE_TYPE})"

# Get current branch
BRANCH_NAME=$(git rev-parse --abbrev-ref HEAD)

info "Committing Loom installation..."

# Stage all changes
git add -A

# Check if there are changes to commit
if git diff --staged --quiet; then
  info "No changes to commit - Loom is already installed"
  info "Skipping commit and PR creation"
  echo "NO_CHANGES_NEEDED"
  exit 0
fi

# Create commit message (use custom if provided, otherwise default install message)
#
# Default subject is prefixed `chore(loom):` so conventional-commit-aware
# tooling can filter; trailer `Skip-CI-Hint: docs-only` is detectable by
# repos that opt in. When SKIP_TARGET_CI=true, prepend `[skip ci]` to the
# subject line (universal GitHub convention).
#
# When COMMIT_MSG is set in the environment, it FULLY OVERRIDES the default
# — no marker injection occurs (composability with custom install scripts).
if [[ -z "${COMMIT_MSG:-}" ]]; then
  commit_subject="chore(loom): Install Loom ${LOOM_VERSION} orchestration framework"
  if [[ "$SKIP_TARGET_CI" == "true" ]]; then
    commit_subject="[skip ci] ${commit_subject}"
  fi
  COMMIT_MSG=$(cat <<EOF
${commit_subject}

Adds Loom configuration and workflow integration:
- .loom/ directory with configuration and scripts
- .claude/commands/loom/ slash commands for roles (/loom/builder, /loom/judge, etc.)
- .claude/settings.json tool permissions
- .github/ labels and workflows
- Documentation (CLAUDE.md)

Loom Version: ${LOOM_VERSION}
Loom Commit: ${LOOM_COMMIT}
Skip-CI-Hint: docs-only
EOF
  )
fi

# Commit changes (redirect output to stderr so it doesn't interfere with PR URL capture)
git commit -m "$COMMIT_MSG" >&2
success "Changes committed"

info "Pushing branch: $BRANCH_NAME"

# Push branch (use --force in case branch exists remotely from previous failed installation)
PUSH_OUTPUT=""
PUSH_EXIT_CODE=0
PUSH_OUTPUT=$(git push -u origin "$BRANCH_NAME" --force 2>&1) || PUSH_EXIT_CODE=$?

if [[ $PUSH_EXIT_CODE -ne 0 ]]; then
  # Check if the error is due to missing workflow scope (GitHub-specific)
  if [[ "$FORGE_TYPE" == "github" ]] && echo "$PUSH_OUTPUT" | grep -q "refusing to allow.*workflow"; then
    echo "" >&2
    echo -e "${YELLOW}⚠ GitHub rejected push: missing 'workflow' scope${NC}" >&2
    echo "" >&2
    info "The GitHub CLI token doesn't have permission to create workflow files."
    info "Retrying without workflow files..."
    echo "" >&2

    # Remove workflow files from the commit and retry
    WORKFLOW_FILES=$(git diff --name-only HEAD~1 HEAD 2>/dev/null | grep "^\.github/workflows/" || true)

    if [[ -n "$WORKFLOW_FILES" ]]; then
      git reset HEAD~1 --soft >&2
      git add -A >&2
      for wf in $WORKFLOW_FILES; do
        if [[ -f "$wf" ]]; then
          git reset HEAD -- "$wf" >&2 2>/dev/null || true
          rm -f "$wf"
        fi
      done

      commit_subject_no_workflow="chore(loom): Install Loom ${LOOM_VERSION} orchestration framework"
      if [[ "$SKIP_TARGET_CI" == "true" ]]; then
        commit_subject_no_workflow="[skip ci] ${commit_subject_no_workflow}"
      fi
      COMMIT_MSG_NO_WORKFLOW=$(cat <<EOF
${commit_subject_no_workflow}

Adds Loom configuration and workflow integration:
- .loom/ directory with configuration and scripts
- .claude/ MCP servers and prompts
- .github/ labels (workflows skipped - requires 'workflow' scope)
- Documentation (CLAUDE.md)

Note: GitHub workflow files were skipped due to missing 'workflow' scope.
To add workflows later, run: gh auth refresh -s workflow

Loom Version: ${LOOM_VERSION}
Loom Commit: ${LOOM_COMMIT}
Skip-CI-Hint: docs-only
EOF
)
      git commit -m "$COMMIT_MSG_NO_WORKFLOW" >&2

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
      echo "$PUSH_OUTPUT" >&2
      error "Failed to push branch: $BRANCH_NAME"
    fi
  else
    echo "$PUSH_OUTPUT" >&2
    error "Failed to push branch: $BRANCH_NAME"
  fi
else
  echo "$PUSH_OUTPUT" >&2
  success "Branch pushed"
fi

info "Creating pull request..."

# Create PR body (use custom if provided, otherwise default install body)
#
# The marker line `loom-install: true` is separator-isolated on its own line
# so trivially greppable from CI scripts that read PR bodies via API. The
# trailing `docs-only: true` line provides a second machine-detectable
# signal. See defaults/docs/ci-integration.md for opt-in patterns.
#
# When PR_BODY is set in the environment, it FULLY OVERRIDES the default —
# no marker injection occurs (composability with custom install scripts).
if [[ -z "${PR_BODY:-}" ]]; then
  PR_BODY=$(cat <<EOF
## Loom Installation

This PR adds Loom orchestration framework to the repository.

**Loom Version**: ${LOOM_VERSION}

## What's Included

- \`.loom/\` - Configuration, roles, and scripts
- \`.claude/commands/loom/\` - Slash commands for roles (/loom/builder, /loom/judge, etc.)
- \`.claude/settings.json\` - Claude Code tool permissions
- \`.github/\` - Labels and workflows
- \`CLAUDE.md\` - Documentation with Loom reference

## Labels

Synced Loom workflow labels via \`.github/labels.yml\`

## Next Steps

After merging:
1. Use \`/builder\`, \`/judge\`, etc. commands in Claude Code
2. Or run \`./.loom/scripts/cli/loom-daemon-start.sh\` to launch the daemon for autonomous orchestration

See \`CLAUDE.md\` for complete usage details.

---

<!-- Loom install markers (see .loom/docs/ci-integration.md for opt-in CI patterns) -->
loom-install: true
docs-only: true

---
Generated by [Loom](https://github.com/rjwalters/loom) installation
EOF
  )
fi

# Use custom PR title if provided, otherwise default install title.
#
# Default title is prefixed `chore(loom):` so conventional-commit-aware CI
# can filter on it. When SKIP_TARGET_CI=true, additionally prepend
# `[skip ci]` (universal GitHub convention) — the opt-in escape hatch.
#
# When PR_TITLE is set in the environment, it FULLY OVERRIDES the default —
# no marker injection occurs.
if [[ -z "${PR_TITLE:-}" ]]; then
  PR_TITLE="chore(loom): Install Loom ${LOOM_VERSION}"
  if [[ "$SKIP_TARGET_CI" == "true" ]]; then
    PR_TITLE="[skip ci] ${PR_TITLE}"
  fi
fi

# ============================================================================
# Create PR - forge-specific
# ============================================================================

PR_URL=""

if [[ "$FORGE_TYPE" == "github" ]]; then
  # Create PR via GitHub CLI
  GH_PR_EXIT=0
  # Pass --head explicitly so gh doesn't try to auto-detect from the local
  # origin remote — that path can fail with "could not resolve remote 'origin'"
  # in shell environments where gh's host detection is degraded, even when
  # -R already pins the target repo (see #3244).
  GH_PR_OUTPUT=$(gh pr create \
    -R "$REPO" \
    --head "$BRANCH_NAME" \
    --base "$BASE_BRANCH" \
    --title "$PR_TITLE" \
    --body "$PR_BODY" \
    --label "loom:pr" 2>&1) || GH_PR_EXIT=$?

  if [[ $GH_PR_EXIT -ne 0 ]]; then
    if echo "$GH_PR_OUTPUT" | grep -qi "already exists"; then
      warning "A pull request already exists for this branch"
      info "Looking up existing PR..."

      EXISTING_PR=$(gh pr list -R "$REPO" --head "$BRANCH_NAME" --base "$BASE_BRANCH" --json url --jq '.[0].url' 2>/dev/null || true)

      if [[ -n "$EXISTING_PR" ]]; then
        success "Found existing PR: $EXISTING_PR"
        PR_URL="$EXISTING_PR"
      else
        echo "Error: PR already exists but could not find its URL" >&2
        echo "gh output was:" >&2
        echo "$GH_PR_OUTPUT" >&2
        exit 1
      fi
    else
      echo "Error: Failed to create pull request" >&2
      echo "gh output was:" >&2
      echo "$GH_PR_OUTPUT" >&2
      exit 1
    fi
  else
    PR_URL=$(echo "$GH_PR_OUTPUT" | grep -oE 'https://[^[:space:]]+/pull/[0-9]+' | head -1 | tr -d '[:space:]')

    if [[ -z "$PR_URL" ]]; then
      echo "Error: Failed to create PR or invalid URL returned" >&2
      echo "gh output was:" >&2
      echo "$GH_PR_OUTPUT" >&2
      exit 1
    fi

    success "Pull request created: $PR_URL"
  fi

elif [[ "$FORGE_TYPE" == "gitea" ]]; then
  # Create PR via Gitea API
  if [[ -z "$FORGE_TOKEN" ]]; then
    error "Gitea API token required to create pull request. Set GITEA_TOKEN or FORGE_TOKEN."
  fi

  PR_PAYLOAD=$(cat <<EOJSON
{
  "title": $(echo "$PR_TITLE" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))'),
  "body": $(echo "$PR_BODY" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))'),
  "head": "$BRANCH_NAME",
  "base": "$BASE_BRANCH",
  "labels": []
}
EOJSON
  )

  GITEA_RESPONSE=$(gitea_api POST "/repos/${FORGE_OWNER}/${FORGE_REPO}/pulls" "$PR_PAYLOAD")
  GITEA_HTTP_CODE=$(echo "$GITEA_RESPONSE" | tail -1)
  GITEA_BODY=$(echo "$GITEA_RESPONSE" | sed '$d')

  if [[ "$GITEA_HTTP_CODE" == "201" ]]; then
    PR_URL=$(echo "$GITEA_BODY" | python3 -c 'import json,sys; data=json.load(sys.stdin); print(data.get("html_url",""))' 2>/dev/null || echo "")
    PR_NUMBER=$(echo "$GITEA_BODY" | python3 -c 'import json,sys; data=json.load(sys.stdin); print(data.get("number",""))' 2>/dev/null || echo "")

    if [[ -n "$PR_URL" ]]; then
      success "Pull request created: $PR_URL"

      # Try to add loom:pr label
      if [[ -n "$PR_NUMBER" ]]; then
        gitea_api POST "/repos/${FORGE_OWNER}/${FORGE_REPO}/issues/${PR_NUMBER}/labels" '{"labels":["loom:pr"]}' > /dev/null 2>&1 || true
      fi
    else
      warning "PR created but could not extract URL from response"
      PR_URL="unknown"
    fi

  elif [[ "$GITEA_HTTP_CODE" == "409" ]]; then
    warning "A pull request already exists for this branch"
    # Try to find existing PR
    EXISTING_RESPONSE=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}/pulls?state=open&head=${FORGE_OWNER}:${BRANCH_NAME}&base=${BASE_BRANCH}")
    EXISTING_BODY=$(echo "$EXISTING_RESPONSE" | sed '$d')
    PR_URL=$(echo "$EXISTING_BODY" | python3 -c 'import json,sys; data=json.load(sys.stdin); print(data[0].get("html_url","") if data else "")' 2>/dev/null || echo "")

    if [[ -n "$PR_URL" ]]; then
      success "Found existing PR: $PR_URL"
    else
      error "PR already exists but could not find its URL"
    fi
  else
    echo "Error: Failed to create pull request (HTTP $GITEA_HTTP_CODE)" >&2
    echo "Response: $GITEA_BODY" >&2
    exit 1
  fi
fi

# ============================================================================
# Attempt to merge the PR (only when FORCE_AUTO_MERGE is enabled)
# ============================================================================
MERGE_STATUS="manual"

if [[ "$FORCE_AUTO_MERGE" == "true" ]]; then
  info "Force mode: Attempting to merge PR..."

  if [[ "$FORGE_TYPE" == "github" ]]; then
    if gh pr merge "$PR_URL" --squash --delete-branch 2>/dev/null; then
      success "PR merged successfully"
      MERGE_STATUS="merged"
    else
      info "Immediate merge not available (ruleset may require reviews)"
      if gh pr merge "$PR_URL" --auto --squash --delete-branch 2>/dev/null; then
        success "Auto-merge enabled - PR will merge once requirements are met"
        MERGE_STATUS="auto"
      else
        warning "Could not merge or enable auto-merge - manual merge required"
        warning "This may be because auto-merge is not enabled on the repository"
        info "To enable: GitHub Settings > General > Allow auto-merge"
        MERGE_STATUS="manual"
      fi
    fi

  elif [[ "$FORGE_TYPE" == "gitea" ]]; then
    # Extract PR number for Gitea merge
    GITEA_PR_NUMBER=$(echo "$PR_URL" | grep -oE '[0-9]+$' || echo "")
    if [[ -n "$GITEA_PR_NUMBER" ]]; then
      MERGE_RESPONSE=$(gitea_api POST "/repos/${FORGE_OWNER}/${FORGE_REPO}/pulls/${GITEA_PR_NUMBER}/merge" '{"Do":"squash","delete_branch_after_merge":true}')
      MERGE_CODE=$(echo "$MERGE_RESPONSE" | tail -1)

      if [[ "$MERGE_CODE" == "200" || "$MERGE_CODE" == "204" ]]; then
        success "PR merged successfully"
        MERGE_STATUS="merged"
      else
        warning "Could not merge PR - manual merge required (HTTP $MERGE_CODE)"
        MERGE_STATUS="manual"
      fi
    else
      warning "Could not extract PR number for merge"
      MERGE_STATUS="manual"
    fi
  fi
else
  info "Pull request created. Review and merge when ready."
fi

# Output the PR URL and merge status (stdout, so it can be captured by caller)
exec 1>&1
printf "%s|%s" "$PR_URL" "$MERGE_STATUS"
