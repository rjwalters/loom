#!/usr/bin/env bash
# Install Loom into a target repository using the full workflow
#
# AGENT USAGE INSTRUCTIONS:
#   This script installs Loom orchestration into a target Git repository.
#
#   Non-interactive mode (for Claude Code):
#     ./scripts/install-loom.sh --yes /path/to/target-repo
#     ./scripts/install-loom.sh -y /path/to/target-repo
#
#   Interactive mode (prompts for confirmations):
#     ./scripts/install-loom.sh /path/to/target-repo
#
#   What this script does:
#     1. Validates target repository (must be a Git repo)
#     2. Creates tracking issue in target repository
#     3. Creates installation worktree (.loom/worktrees/issue-XXX)
#     4. Initializes Loom configuration (copies defaults to .loom/)
#     5. Syncs GitHub labels for Loom workflow
#     6. Configures branch protection rules (interactive mode only)
#     7. Creates pull request with loom:review-requested label
#
#   Requirements:
#     - Target must be a Git repository
#     - GitHub CLI (gh) must be authenticated
#     - loom-daemon binary must be built (pnpm daemon:build)
#
#   After installation:
#     - Merge the generated PR in the target repository
#     - Loom will be ready to use in that workspace

set -euo pipefail

# Parse command line arguments
NON_INTERACTIVE=false
TARGET_PATH=""

while [[ $# -gt 0 ]]; do
  case $1 in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    *)
      TARGET_PATH="$1"
      shift
      ;;
  esac
done

# Determine Loom repository root (where this script lives)
LOOM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

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

warning() {
  echo -e "${YELLOW}⚠ Warning: $*${NC}"
}

header() {
  echo -e "${CYAN}$*${NC}"
}

# Cleanup function called on error
cleanup_on_error() {
  local exit_code=$?
  if [[ $exit_code -ne 0 ]]; then
    echo ""
    warning "Installation failed at step: ${CURRENT_STEP:-unknown}"

    if [[ -n "${ISSUE_NUMBER:-}" ]]; then
      info "Cleaning up tracking issue #${ISSUE_NUMBER}..."
      cd "$TARGET_PATH" 2>/dev/null || true
      # Detect repo for gh commands (best effort, ignore errors in cleanup)
      CLEANUP_REPO=$(git config --get remote.origin.url 2>/dev/null | sed -E 's#^.*(github\.com[/:])##; s/\.git$//' || echo "")
      if [[ -n "$CLEANUP_REPO" ]] && [[ "$CLEANUP_REPO" =~ ^[^/]+/[^/]+$ ]]; then
        gh issue close "${ISSUE_NUMBER}" -R "$CLEANUP_REPO" --comment "Installation failed during setup. Please retry." 2>/dev/null || true
      else
        gh issue close "${ISSUE_NUMBER}" --comment "Installation failed during setup. Please retry." 2>/dev/null || true
      fi
    fi

    if [[ -n "${WORKTREE_PATH:-}" ]] && [[ -d "${TARGET_PATH}/${WORKTREE_PATH}" ]]; then
      info "Cleaning up worktree: ${WORKTREE_PATH}..."
      cd "$TARGET_PATH" 2>/dev/null || true
      git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
      if [[ -n "${BRANCH_NAME:-}" ]]; then
        git branch -D "${BRANCH_NAME}" 2>/dev/null || true
      fi
    fi

    echo ""
    error "Installation did not complete. See above for details."
  fi
}

trap cleanup_on_error EXIT SIGINT SIGTERM

# Validate arguments
if [[ -z "$TARGET_PATH" ]]; then
  error "Target repository path required\nUsage: $0 [--yes|-y] /path/to/target-repo"
fi

# Export for sub-scripts
export NON_INTERACTIVE

# Resolve target to absolute path (git repository root, not worktree)
TARGET_PATH="$(cd "$TARGET_PATH" 2>/dev/null && pwd)" || \
  error "Target path does not exist: $TARGET_PATH"

# Check if target is a git repository
if ! git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  error "Target path is not a git repository: $TARGET_PATH"
fi

# If target is inside a worktree, resolve to the main repository root
# git worktree list --porcelain returns the main worktree first
MAIN_WORKTREE=$(git -C "$TARGET_PATH" worktree list --porcelain 2>/dev/null | grep -m1 '^worktree ' | cut -d' ' -f2-)
if [[ -n "$MAIN_WORKTREE" ]] && [[ "$TARGET_PATH" != "$MAIN_WORKTREE" ]]; then
  warning "Target path is inside a worktree: $TARGET_PATH"
  info "Resolving to main repository root: $MAIN_WORKTREE"
  TARGET_PATH="$MAIN_WORKTREE"
fi

echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║           Loom Installation - Full Workflow               ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

info "Target: $TARGET_PATH"
echo ""

# Check required dependencies
header "Checking Dependencies"
echo ""

MISSING_DEPS=()

# Check if node is available (needed for version extraction)
if command -v node &> /dev/null; then
  success "node: $(node --version)"
else
  MISSING_DEPS+=("node")
fi

# Check for pnpm (needed to build daemon)
if command -v pnpm &> /dev/null; then
  success "pnpm: $(pnpm --version)"
else
  MISSING_DEPS+=("pnpm")
fi

# Check for cargo (needed to build daemon)
if command -v cargo &> /dev/null; then
  success "cargo: $(cargo --version | head -1)"
else
  MISSING_DEPS+=("cargo")
fi

echo ""

# Report missing dependencies
if [[ ${#MISSING_DEPS[@]} -gt 0 ]]; then
  echo -e "${RED}✗ Missing required dependencies: ${MISSING_DEPS[*]}${NC}"
  echo ""
  info "Please install the missing dependencies:"
  for dep in "${MISSING_DEPS[@]}"; do
    case "$dep" in
      node)
        echo "  • Node.js: brew install node (or https://nodejs.org/)"
        ;;
      pnpm)
        echo "  • pnpm: npm install -g pnpm (or https://pnpm.io/installation)"
        ;;
      cargo)
        echo "  • Rust/Cargo: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        ;;
    esac
  done
  echo ""
  error "Cannot proceed without required dependencies"
fi

# Extract Loom version from package.json
if [[ ! -f "$LOOM_ROOT/package.json" ]]; then
  error "Cannot find package.json in Loom repository"
fi

LOOM_VERSION=$(node -pe "require('$LOOM_ROOT/package.json').version" 2>/dev/null) || \
  error "Failed to extract version from package.json"

# Extract Loom commit hash
cd "$LOOM_ROOT"
LOOM_COMMIT=$(git rev-parse --short HEAD 2>/dev/null) || \
  error "Failed to get git commit hash"

success "Loom Version: v${LOOM_VERSION}"
success "Loom Commit: ${LOOM_COMMIT}"
echo ""

# Export environment variables for installation scripts
export LOOM_VERSION
export LOOM_COMMIT
export LOOM_ROOT

# Verify installation scripts directory exists
if [[ ! -d "$LOOM_ROOT/scripts/install" ]]; then
  error "Installation scripts directory not found: $LOOM_ROOT/scripts/install"
fi

# ============================================================================
# STEP 1: Validate Target Repository
# ============================================================================
CURRENT_STEP="Validate Target"
header "Step 1: Validating Target Repository"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/validate-target.sh" ]]; then
  error "Installation script not found: validate-target.sh"
fi

"$LOOM_ROOT/scripts/install/validate-target.sh" "$TARGET_PATH" || \
  error "Target validation failed"

# Check for workflow scope (non-blocking warning)
if ! "$LOOM_ROOT/scripts/install/check-workflow-scope.sh" 2>/dev/null; then
  echo ""
  warning "GitHub CLI token is missing 'workflow' scope"
  info "Workflow files (.github/workflows/) may be skipped during installation."
  echo ""
  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    info "Continuing in non-interactive mode - workflows will be skipped if needed."
  else
    echo "  To add the workflow scope now, run:"
    echo "    gh auth refresh -s workflow"
    echo ""
    read -p "Continue without workflow scope? (Y/n) " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Nn]$ ]]; then
      echo ""
      info "Run 'gh auth refresh -s workflow' and retry installation."
      exit 0
    fi
  fi
fi

echo ""

# ============================================================================
# STEP 2: Create Tracking Issue
# ============================================================================
CURRENT_STEP="Create Issue"
header "Step 2: Creating Tracking Issue"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-issue.sh" ]]; then
  error "Installation script not found: create-issue.sh"
fi

ISSUE_NUMBER=$("$LOOM_ROOT/scripts/install/create-issue.sh" "$TARGET_PATH") || \
  error "Failed to create tracking issue"

if [[ ! "$ISSUE_NUMBER" =~ ^[0-9]+$ ]]; then
  error "Invalid issue number returned: $ISSUE_NUMBER"
fi

info "Tracking issue: #${ISSUE_NUMBER}"
echo ""

# ============================================================================
# STEP 3: Create Installation Worktree
# ============================================================================
CURRENT_STEP="Create Worktree"
header "Step 3: Creating Installation Worktree"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-worktree.sh" ]]; then
  error "Installation script not found: create-worktree.sh"
fi

WORKTREE_OUTPUT=$("$LOOM_ROOT/scripts/install/create-worktree.sh" "$TARGET_PATH" "$ISSUE_NUMBER") || \
  error "Failed to create worktree"

# Parse output: WORKTREE_PATH|BRANCH_NAME|BASE_BRANCH
WORKTREE_PATH=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f1)
BRANCH_NAME=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f2)
BASE_BRANCH=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f3)

# Validate worktree path format (should be relative path starting with .loom/worktrees/)
if [[ ! "$WORKTREE_PATH" =~ ^\.loom/worktrees/ ]]; then
  error "Invalid worktree path returned: $WORKTREE_PATH"
fi

if [[ ! -d "$TARGET_PATH/$WORKTREE_PATH" ]]; then
  error "Worktree was not created: $TARGET_PATH/$WORKTREE_PATH"
fi

info "Worktree: $WORKTREE_PATH"
info "Branch: $BRANCH_NAME"
info "Base branch: $BASE_BRANCH"
echo ""

# ============================================================================
# STEP 4: Initialize Loom Configuration
# ============================================================================
CURRENT_STEP="Initialize Loom"
header "Step 4: Initializing Loom Configuration"
echo ""

# Check if loom-daemon is built
if [[ ! -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
  warning "loom-daemon binary not found"
  info "Building loom-daemon (this may take a minute)..."

  # Check if pnpm is available
  if ! command -v pnpm &> /dev/null; then
    error "pnpm is required to build loom-daemon but not found in PATH\n       Install from: https://pnpm.io/installation"
  fi

  cd "$LOOM_ROOT"
  pnpm daemon:build || error "Failed to build loom-daemon"
  echo ""
fi

success "loom-daemon binary ready"

# Run loom-daemon init in the worktree
cd "$TARGET_PATH/$WORKTREE_PATH"
# Use --force in case .loom already exists in the target repo
# Use --defaults to point to Loom's defaults directory
"$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" . || \
  error "loom-daemon init failed"

echo ""

# Verify expected files were created
EXPECTED_FILES=(
  ".loom/config.json"
  ".loom/roles"
  ".loom/scripts/worktree.sh"
  "CLAUDE.md"
  ".github/labels.yml"
  ".claude/commands"
  ".claude/settings.json"
)

info "Verifying installation files..."
for file in "${EXPECTED_FILES[@]}"; do
  if [[ ! -e "$file" ]]; then
    error "Expected file not created: $file"
  fi
done

success "All Loom files installed"
echo ""

# Install cleanup scripts to .loom/scripts/
info "Installing cleanup scripts..."
cp "$LOOM_ROOT/scripts/cleanup.sh" ".loom/scripts/cleanup.sh" || \
  error "Failed to copy cleanup.sh"
cp "$LOOM_ROOT/scripts/cleanup-branches.sh" ".loom/scripts/cleanup-branches.sh" || \
  error "Failed to copy cleanup-branches.sh"
chmod +x ".loom/scripts/cleanup.sh"
chmod +x ".loom/scripts/cleanup-branches.sh"
success "✓ Installed cleanup scripts to .loom/scripts/"
echo ""

# ============================================================================
# STEP 5: Sync GitHub Labels
# ============================================================================
CURRENT_STEP="Sync Labels"
header "Step 5: Syncing GitHub Labels"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/sync-labels.sh" ]]; then
  error "Installation script not found: sync-labels.sh"
fi

"$LOOM_ROOT/scripts/install/sync-labels.sh" "$TARGET_PATH/$WORKTREE_PATH" || \
  warning "Label sync had issues but continuing..."

echo ""

# Now that labels are synced, ensure the tracking issue has the loom:building label
info "Ensuring tracking issue has loom:building label..."
cd "$TARGET_PATH"
# Detect the target repository from git remote
TARGET_REPO=$(git config --get remote.origin.url 2>/dev/null | sed -E 's#^.*(github\.com[/:])##; s/\.git$//' || echo "")
if [[ -n "$TARGET_REPO" ]] && [[ "$TARGET_REPO" =~ ^[^/]+/[^/]+$ ]]; then
  if gh issue edit "$ISSUE_NUMBER" -R "$TARGET_REPO" --add-label "loom:building" >/dev/null 2>&1; then
    success "Added loom:building label to issue #${ISSUE_NUMBER}"
  else
    warning "Could not add loom:building label to issue #${ISSUE_NUMBER}"
  fi
else
  warning "Could not detect repository for label update"
fi

echo ""

# ============================================================================
# STEP 6: Configure Branch Protection
# ============================================================================
CURRENT_STEP="Configure Branch Protection"
header "Step 6: Configure Branch Protection"
echo ""

# Detect default branch
cd "$TARGET_PATH"
DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "main")
info "Detected default branch: ${DEFAULT_BRANCH}"

# Prompt user
if [[ "$NON_INTERACTIVE" == "true" ]]; then
  info "Non-interactive mode: Skipping branch protection setup"
  info "To configure manually, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
else
  echo ""
  read -p "Configure branch protection rules for '${DEFAULT_BRANCH}' branch? (y/N) " -n 1 -r
  echo ""
  if [[ $REPLY =~ ^[Yy]$ ]]; then
    info "Applying branch protection rules..."

    # Apply protection rules
    if "$LOOM_ROOT/scripts/install/setup-branch-protection.sh" "$TARGET_PATH" "$DEFAULT_BRANCH"; then
      echo ""
    else
      echo ""
      warning "Failed to configure branch protection (may require admin permissions)"
      info "You can configure manually via GitHub Settings > Branches"
    fi
  else
    info "Skipping branch protection setup"
    info "To configure later, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
  fi
fi

echo ""

# ============================================================================
# STEP 7: Create Pull Request
# ============================================================================
CURRENT_STEP="Create PR"
header "Step 7: Creating Pull Request"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-pr.sh" ]]; then
  error "Installation script not found: create-pr.sh"
fi

PR_URL_RAW=$("$LOOM_ROOT/scripts/install/create-pr.sh" "$TARGET_PATH/$WORKTREE_PATH" "$ISSUE_NUMBER" "$BASE_BRANCH") || \
  error "Failed to create pull request"

# Check if installation was already complete (no changes needed)
# IMPORTANT: Check this BEFORE trying to parse as URL
if [[ "$PR_URL_RAW" == *"NO_CHANGES_NEEDED"* ]]; then
  info "Loom is already installed - cleaning up..."

  # Close the tracking issue
  cd "$TARGET_PATH"
  # Detect the target repository from git remote
  TARGET_REPO=$(git config --get remote.origin.url 2>/dev/null | sed -E 's#^.*(github\.com[/:])##; s/\.git$//' || echo "")
  if [[ -n "$TARGET_REPO" ]] && [[ "$TARGET_REPO" =~ ^[^/]+/[^/]+$ ]]; then
    gh issue close "${ISSUE_NUMBER}" -R "$TARGET_REPO" --comment "Loom is already installed. No changes needed." 2>/dev/null || true
  else
    gh issue close "${ISSUE_NUMBER}" --comment "Loom is already installed. No changes needed." 2>/dev/null || true
  fi

  # Remove the worktree
  git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
  git branch -D "${BRANCH_NAME}" 2>/dev/null || true

  # Disable error trap and exit successfully
  trap - EXIT SIGINT SIGTERM

  echo ""
  success "Loom is already installed in this repository"
  echo ""
  info "No pull request was created because all files are up to date."
  echo ""
  exit 0
fi

# Extract just the URL from the output in case there's any extra content
# This handles cases where gh or other commands leak output to stdout
PR_URL=$(echo "$PR_URL_RAW" | grep -oE 'https://github\.com/[^[:space:]]+/pull/[0-9]+' | head -1 | tr -d '[:space:]')

if [[ ! "$PR_URL" =~ ^https:// ]]; then
  error "Invalid PR URL returned: $PR_URL"
fi

success "Pull request created"
echo ""

# ============================================================================
# Installation Complete
# ============================================================================
CURRENT_STEP="Complete"

# Disable error trap - we completed successfully
trap - EXIT SIGINT SIGTERM

echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║              ✓ Installation Complete!                    ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

success "Loom ${LOOM_VERSION} installed successfully"
echo ""

info "📋 Tracking Issue: #${ISSUE_NUMBER}"
info "📦 Pull Request: ${PR_URL}"
echo ""

header "What's Included:"
echo "  ✅ .loom/ directory with configuration and scripts"
echo "  ✅ .claude/ directory with slash commands"
echo "  ✅ .github/ directory with labels and workflows"
echo "  ✅ CLAUDE.md and AGENTS.md documentation"
echo ""

header "Next Steps:"
echo "  1. Review and merge the pull request: ${PR_URL}"
echo "  2. Choose your workflow:"
echo "     Manual Mode (recommended to start):"
echo "       cd $TARGET_PATH && claude"
echo "       Then use /builder, /judge, or other role commands"
echo "     Tauri App Mode (requires Loom.app - see README):"
echo "       Download Loom.app from releases, open workspace"
echo ""

info "See CLAUDE.md in the target repository for complete usage details."
echo ""
