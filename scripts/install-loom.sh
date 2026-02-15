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
#     2. Creates installation worktree (.loom/worktrees/loom-installation)
#     3. Initializes Loom configuration (copies defaults to .loom/)
#     4. Syncs GitHub labels for Loom workflow
#     5. Configures branch rulesets (interactive mode only)
#     6. Creates pull request with loom:review-requested label
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
FORCE_OVERWRITE=false
CLEAN_FIRST=false
TARGET_PATH=""

while [[ $# -gt 0 ]]; do
  case $1 in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    -f|--force)
      FORCE_OVERWRITE=true
      shift
      ;;
    --clean)
      CLEAN_FIRST=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [OPTIONS] /path/to/target-repo"
      echo ""
      echo "Options:"
      echo "  -y, --yes     Non-interactive mode"
      echo "  -f, --force   Force overwrite existing files and enable auto-merge"
      echo "  --clean       Run uninstall first, then fresh install (combines both operations)"
      echo "  -h, --help    Show this help message"
      exit 0
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

trap cleanup_on_error EXIT
trap 'exit 130' SIGINT
trap 'exit 143' SIGTERM

# Validate arguments
if [[ -z "$TARGET_PATH" ]]; then
  error "Target repository path required\nUsage: $0 [--yes|-y] /path/to/target-repo"
fi

# Export for sub-scripts
export NON_INTERACTIVE

# Resolve target to absolute path (git repository root, not worktree)
TARGET_PATH="$(cd "$TARGET_PATH" 2>/dev/null && pwd)" || \
  error "Target path does not exist: $TARGET_PATH"

# Check if target is a git repository - offer to initialize if not
if ! git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    # In non-interactive mode, require --init-git flag for git initialization
    error "Target path is not a git repository: $TARGET_PATH\n       Use interactive mode to initialize git, or run 'git init' first."
  fi

  echo ""
  warning "$TARGET_PATH is not a git repository."
  echo ""
  echo "Would you like to initialize git and set up GitHub?"
  echo ""
  echo "This will:"
  echo "  1. Run 'git init' in the directory"
  echo "  2. Create a sensible .gitignore file"
  echo "  3. Create an initial commit"
  echo "  4. Create a GitHub repository and set up remote (required for Full Install)"
  echo ""
  read -r -p "Initialize git and GitHub? [y/N] " -n 1 INIT_GIT
  echo ""

  if [[ ! $INIT_GIT =~ ^[Yy]$ ]]; then
    error "Full Install requires a git repository with GitHub remote.\n       Run 'git init' and 'gh repo create' manually, or use Quick Install."
  fi

  # Initialize git
  info "Initializing git repository..."
  cd "$TARGET_PATH"
  git init --quiet || error "Failed to initialize git repository"
  success "Git repository initialized"

  # Create basic .gitignore if it doesn't exist
  if [[ ! -f "$TARGET_PATH/.gitignore" ]]; then
    info "Creating .gitignore..."
    cat > "$TARGET_PATH/.gitignore" << 'GITIGNORE'
# Dependencies
node_modules/
vendor/

# Build outputs
dist/
build/
target/
*.o
*.a
*.so
*.dylib

# IDE/Editor
.idea/
.vscode/
*.swp
*.swo
*~

# OS files
.DS_Store
Thumbs.db

# Environment files
.env
.env.local
.env.*.local

# Logs
*.log
logs/

# Loom runtime state (added by loom-daemon init)
# These patterns are also managed by loom-daemon's update_gitignore().
GITIGNORE
    success "Created .gitignore"
  fi

  # Create initial commit
  info "Creating initial commit..."
  git add -A
  git commit -m "Initial commit" --quiet || error "Failed to create initial commit"
  success "Initial commit created"
  echo ""

  # GitHub repository creation is required for Full Install
  info "Full Install requires a GitHub repository."
  echo ""

  # Check GitHub authentication
  if ! gh auth status &> /dev/null; then
    warning "GitHub CLI is not authenticated"
    info "Please authenticate with GitHub:"
    echo ""
    gh auth login || error "GitHub authentication failed"
    echo ""
  fi

  # Prompt for repository visibility
  echo "Repository visibility:"
  echo "  1. Private (default)"
  echo "  2. Public"
  read -r -p "Choose visibility [1/2]: " -n 1 VISIBILITY
  echo ""

  VISIBILITY_FLAG="--private"
  if [[ "$VISIBILITY" == "2" ]]; then
    VISIBILITY_FLAG="--public"
  fi

  # Get directory name for repo name suggestion
  DIR_NAME=$(basename "$TARGET_PATH")
  read -r -p "Repository name [$DIR_NAME]: " REPO_NAME
  REPO_NAME="${REPO_NAME:-$DIR_NAME}"

  info "Creating GitHub repository: $REPO_NAME..."
  if gh repo create "$REPO_NAME" $VISIBILITY_FLAG --source="$TARGET_PATH" --push; then
    success "GitHub repository created and pushed"
  else
    error "Failed to create GitHub repository. Cannot proceed with Full Install."
  fi
  echo ""
fi

# If target is inside a worktree, resolve to the main repository root
# git worktree list --porcelain returns the main worktree first
# Note: Use head -4 before grep to avoid SIGPIPE when repo has many worktrees
# (grep -m1 exits early, causing SIGPIPE to git with pipefail enabled)
MAIN_WORKTREE=$(git -C "$TARGET_PATH" worktree list --porcelain 2>/dev/null | head -4 | grep -m1 '^worktree ' | cut -d' ' -f2- || true)
if [[ -n "$MAIN_WORKTREE" ]] && [[ "$TARGET_PATH" != "$MAIN_WORKTREE" ]]; then
  warning "Target path is inside a worktree: $TARGET_PATH"
  info "Resolving to main repository root: $MAIN_WORKTREE"
  TARGET_PATH="$MAIN_WORKTREE"
fi

# ============================================================================
# PRE-INSTALLATION: Clean existing installation if --clean flag was passed
# ============================================================================
if [[ "$CLEAN_FIRST" == "true" ]]; then
  header "Pre-Installation: Running Uninstall"
  echo ""

  # Check if Loom is installed (has .loom directory)
  if [[ -d "$TARGET_PATH/.loom" ]]; then
    # Preserve uncommitted user changes before uninstall
    # The uninstall --local mode runs 'git add -A' which would stage user changes
    # along with uninstall deletions, and our cleanup would discard them
    STASHED_USER_CHANGES=false
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      warning "Working tree has uncommitted changes"
      info "Stashing user changes to preserve them during clean install..."
      if git -C "$TARGET_PATH" stash push -m "loom-install: preserving user changes before --clean" 2>/dev/null; then
        STASHED_USER_CHANGES=true
        success "User changes stashed"
      else
        warning "Failed to stash changes - uncommitted changes may be lost during --clean install"
        warning "Consider committing your changes first, then retry"
      fi
    fi

    info "Running local uninstall to clean existing installation..."

    # Build uninstall flags from current flags
    # Always use --local so removal happens in working directory (not a worktree)
    # This way the install worktree captures both removals and additions in one PR
    # Always use --clean so unknown files in managed directories are removed
    UNINSTALL_FLAGS="--local --clean"
    if [[ "$NON_INTERACTIVE" == "true" ]]; then
      UNINSTALL_FLAGS="$UNINSTALL_FLAGS --yes"
    fi

    # Run uninstall script in local mode
    "$LOOM_ROOT/scripts/uninstall-loom.sh" $UNINSTALL_FLAGS "$TARGET_PATH" || \
      error "Clean install failed - uninstall step encountered an error"

    echo ""
    success "Uninstall complete - proceeding with fresh installation"

    # Clean up staged deletions from uninstall
    # The uninstall ran in --local mode, staging file deletions directly in the
    # main working directory. The fresh install will happen in a worktree, so
    # main must stay clean to avoid leftover staged changes after completion.
    info "Cleaning staged changes from uninstall..."

    CLEANUP_FAILED=false

    if ! git -C "$TARGET_PATH" restore --staged . 2>/dev/null; then
      warning "Failed to unstage changes from uninstall"
      CLEANUP_FAILED=true
    fi

    if ! git -C "$TARGET_PATH" checkout -- . 2>/dev/null; then
      warning "Failed to restore files from uninstall"
      CLEANUP_FAILED=true
    fi

    # Also clean any untracked files left by the uninstall process
    # (only remove files in Loom-managed directories, not user files)
    git -C "$TARGET_PATH" clean -fd .loom/ .claude/ .codex/ .github/labels.yml 2>/dev/null || true

    # Verify the working tree is clean
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      CLEANUP_FAILED=true
    fi

    if [[ "$CLEANUP_FAILED" == "true" ]]; then
      echo ""
      warning "Working tree may not be fully clean after uninstall cleanup"
      warning "Remaining changes:"
      git -C "$TARGET_PATH" status --short 2>/dev/null || true
      echo ""
      warning "To fix manually after installation:"
      warning "  cd $TARGET_PATH"
      warning "  git restore --staged ."
      warning "  git checkout -- ."
    else
      success "Working tree is clean after uninstall cleanup"
    fi

    # Restore stashed user changes
    if [[ "$STASHED_USER_CHANGES" == "true" ]]; then
      info "Restoring stashed user changes..."
      if git -C "$TARGET_PATH" stash pop 2>/dev/null; then
        success "User changes restored"
      else
        warning "Failed to restore stashed user changes automatically"
        warning "Run 'git stash pop' in $TARGET_PATH to recover your changes"
      fi
    fi

    echo ""
  else
    info "No existing Loom installation detected - proceeding with fresh install"
    echo ""
  fi
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

# Check for Python 3.10+ (needed for loom-tools)
PYTHON_FOUND=false
for py in python3.12 python3.11 python3.10 python3; do
  if command -v "$py" &>/dev/null; then
    version=$("$py" --version 2>&1 | grep -oE '[0-9]+\.[0-9]+' | head -1)
    major=$(echo "$version" | cut -d. -f1)
    minor=$(echo "$version" | cut -d. -f2)
    if [[ "$major" -ge 3 ]] && [[ "$minor" -ge 10 ]]; then
      success "python: $("$py" --version 2>&1)"
      PYTHON_FOUND=true
      break
    fi
  fi
done
if [[ "$PYTHON_FOUND" != "true" ]]; then
  MISSING_DEPS+=("python3.10+")
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
      python3.10+)
        echo "  • Python 3.10+: brew install python@3.12 (or https://python.org/)"
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

# Export FORCE_AUTO_MERGE for create-pr.sh
# When --force is passed, also enable auto-merge on the installation PR
if [[ "$FORCE_OVERWRITE" == "true" ]]; then
  export FORCE_AUTO_MERGE=true
else
  export FORCE_AUTO_MERGE=false
fi

# Verify installation scripts directory exists
if [[ ! -d "$LOOM_ROOT/scripts/install" ]]; then
  error "Installation scripts directory not found: $LOOM_ROOT/scripts/install"
fi

# ============================================================================
# PRE-FLIGHT: Idempotency Checks
# ============================================================================
# Skip idempotency checks if --force or --clean flags are set
if [[ "$FORCE_OVERWRITE" != "true" ]] && [[ "$CLEAN_FIRST" != "true" ]]; then
  header "Pre-flight: Idempotency Checks"
  echo ""

  # Check 1: Is Loom already installed with the same version?
  if [[ -f "$TARGET_PATH/CLAUDE.md" ]]; then
    INSTALLED_VERSION=$(grep 'Loom Version' "$TARGET_PATH/CLAUDE.md" 2>/dev/null | head -1 | sed 's/.*Loom Version.*: //' | sed 's/\*//g' | tr -d '[:space:]' || true)
    if [[ "$INSTALLED_VERSION" == "$LOOM_VERSION" ]]; then
      info "Loom v${LOOM_VERSION} is already installed in this repository."
      info "Use --force to reinstall or --clean for a fresh install."
      echo ""

      # Disable error trap and exit successfully
      trap - EXIT SIGINT SIGTERM
      exit 0
    elif [[ -n "$INSTALLED_VERSION" ]]; then
      info "Existing Loom installation detected: v${INSTALLED_VERSION}"
      info "Upgrading to: v${LOOM_VERSION}"
    fi
  fi

  # Check 2: Is there already an open installation PR?
  REPO_NWOPATH=$(git -C "$TARGET_PATH" config --get remote.origin.url 2>/dev/null | sed -E 's#^.*(github\.com[/:])##; s/\.git$//' || true)
  if [[ -n "$REPO_NWOPATH" ]] && [[ "$REPO_NWOPATH" =~ ^[^/]+/[^/]+$ ]]; then
    EXISTING_INSTALL_PR=$(gh pr list -R "$REPO_NWOPATH" --state open --search "Install Loom" --json url,headRefName --jq '
      [.[] | select(.headRefName | startswith("feature/loom-installation"))][0].url' 2>/dev/null || true)

    if [[ -n "$EXISTING_INSTALL_PR" ]]; then
      warning "An open Loom installation PR already exists:"
      info "  ${EXISTING_INSTALL_PR}"
      echo ""

      if [[ "$NON_INTERACTIVE" == "true" ]]; then
        info "Non-interactive mode: Skipping duplicate installation."
        info "Use --force to create a new PR regardless."
        echo ""
        trap - EXIT SIGINT SIGTERM
        exit 0
      else
        read -r -p "Create a new installation PR anyway? [y/N] " -n 1 CREATE_ANYWAY
        echo ""
        if [[ ! $CREATE_ANYWAY =~ ^[Yy]$ ]]; then
          info "Aborting. Merge or close the existing PR first, or use --force."
          trap - EXIT SIGINT SIGTERM
          exit 0
        fi
      fi
    fi
  fi

  success "Idempotency checks passed"
  echo ""
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
# STEP 2: Create Installation Worktree
# ============================================================================
CURRENT_STEP="Create Worktree"
header "Step 2: Creating Installation Worktree"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-worktree.sh" ]]; then
  error "Installation script not found: create-worktree.sh"
fi

WORKTREE_OUTPUT=$("$LOOM_ROOT/scripts/install/create-worktree.sh" "$TARGET_PATH") || \
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
# STEP 3: Initialize Loom Configuration
# ============================================================================
CURRENT_STEP="Initialize Loom"
header "Step 3: Initializing Loom Configuration"
echo ""

# Build loom-daemon (always rebuild to ensure binary matches current source)
# Cargo's incremental compilation makes this fast (~1-2s) when nothing changed
info "Building loom-daemon..."
cd "$LOOM_ROOT"
pnpm daemon:build || error "Failed to build loom-daemon"

success "loom-daemon binary ready"

# Run loom-daemon init in the worktree
cd "$TARGET_PATH/$WORKTREE_PATH"
# Use --force to overwrite existing installation when requested (--force or --clean flags)
# Otherwise, use merge mode to preserve custom project roles/commands
# Use --defaults to point to Loom's defaults directory
INIT_FLAGS=""
if [ "$FORCE_OVERWRITE" = true ] || [ "$CLEAN_FIRST" = true ]; then
  INIT_FLAGS="--force"
fi
"$LOOM_ROOT/target/release/loom-daemon" init $INIT_FLAGS --defaults "$LOOM_ROOT/defaults" . || \
  error "loom-daemon init failed"

echo ""

# Copy hooks to target (settings.json references .loom/hooks/guard-destructive.sh)
info "Installing hooks..."
if [[ -d "$LOOM_ROOT/defaults/hooks" ]]; then
  mkdir -p .loom/hooks
  for hook_file in "$LOOM_ROOT/defaults/hooks/"*.sh; do
    [[ -f "$hook_file" ]] || continue
    hook_name=$(basename "$hook_file")
    if [[ -f ".loom/hooks/$hook_name" ]] && [[ "$FORCE_OVERWRITE" != "true" ]] && [[ "$CLEAN_FIRST" != "true" ]]; then
      info "Skipping existing hook: $hook_name (use --force to overwrite)"
    else
      cp "$hook_file" ".loom/hooks/$hook_name"
      chmod +x ".loom/hooks/$hook_name"
      success "Installed hook: $hook_name"
    fi
  done
else
  warning "No hooks directory found in defaults"
fi
echo ""

# Set up Python tools (loom-tools package)
# This creates a virtual environment in loom-tools/.venv and installs loom-shepherd, etc.
info "Setting up Python tools..."
if [[ -x "$LOOM_ROOT/scripts/install/setup-python-tools.sh" ]]; then
  if "$LOOM_ROOT/scripts/install/setup-python-tools.sh" --loom-root "$LOOM_ROOT"; then
    success "Python tools installed"
  else
    warning "Python tools setup failed (non-fatal for installation)"
    info "Run manually: $LOOM_ROOT/scripts/install/setup-python-tools.sh --loom-root $LOOM_ROOT"
    info "Without Python tools, /shepherd and some scripts will not work."
  fi
else
  warning "Python setup script not found"
  info "Python tools (loom-shepherd, etc.) may not be available."
fi

# Store Loom source repository path for wrapper scripts
# This enables scripts in the target repo to find loom-tools in the source repo
info "Recording Loom source path..."
echo "$LOOM_ROOT" > .loom/loom-source-path
# Also write to target repo root — the worktree copy is gitignored and will be
# lost when the installation worktree is cleaned up after PR merge
echo "$LOOM_ROOT" > "$TARGET_PATH/.loom/loom-source-path"
success "Loom source path recorded"

# Store installation metadata (commit hash moved here from CLAUDE.md for idempotency)
info "Recording installation metadata..."
cat > .loom/install-metadata.json <<METADATA
{
  "loom_version": "${LOOM_VERSION}",
  "loom_commit": "${LOOM_COMMIT}",
  "install_date": "$(date +%Y-%m-%d)",
  "loom_source": "${LOOM_ROOT}"
}
METADATA
success "Installation metadata recorded"
echo ""

# Verify expected files were created
EXPECTED_FILES=(
  ".loom/config.json"
  ".loom/roles"
  ".loom/scripts/worktree.sh"
  ".loom/hooks/guard-destructive.sh"
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

# Verify installed scripts match source defaults
info "Verifying scripts match source..."
VERIFY_FAILURES=0
if [[ -d "$LOOM_ROOT/defaults/scripts" ]] && [[ -d ".loom/scripts" ]]; then
  while IFS= read -r -d '' src_file; do
    rel_path="${src_file#$LOOM_ROOT/defaults/scripts/}"
    dst_file=".loom/scripts/$rel_path"
    if [[ -f "$dst_file" ]]; then
      if ! cmp -s "$src_file" "$dst_file"; then
        warning "Script mismatch: .loom/scripts/$rel_path differs from source"
        VERIFY_FAILURES=$((VERIFY_FAILURES + 1))
      fi
    else
      warning "Script missing: .loom/scripts/$rel_path not installed"
      VERIFY_FAILURES=$((VERIFY_FAILURES + 1))
    fi
  done < <(find "$LOOM_ROOT/defaults/scripts" -type f -print0)
fi
if [[ $VERIFY_FAILURES -gt 0 ]]; then
  warning "$VERIFY_FAILURES script(s) failed verification — see above"
else
  success "All scripts verified (match source defaults)"
fi
echo ""

# Install Loom CLI wrapper (.loom/bin/loom)
if [[ -f "$LOOM_ROOT/defaults/.loom/bin/loom" ]]; then
  info "Installing Loom CLI wrapper..."
  mkdir -p ".loom/bin"
  cp "$LOOM_ROOT/defaults/.loom/bin/loom" ".loom/bin/loom" || \
    error "Failed to copy Loom CLI"
  chmod +x ".loom/bin/loom"
  success "Installed .loom/bin/loom CLI"
fi
echo ""

# Generate installation manifest (checksum of all installed files)
if [[ -x ".loom/scripts/verify-install.sh" ]]; then
  info "Generating installation manifest..."
  ./.loom/scripts/verify-install.sh generate --quiet || \
    warning "Manifest generation failed (non-fatal)"
  success "Installation manifest generated (.loom/manifest.json)"
fi
echo ""

# ============================================================================
# STEP 4: Sync GitHub Labels
# ============================================================================
CURRENT_STEP="Sync Labels"
header "Step 4: Syncing GitHub Labels"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/sync-labels.sh" ]]; then
  error "Installation script not found: sync-labels.sh"
fi

"$LOOM_ROOT/scripts/install/sync-labels.sh" "$TARGET_PATH/$WORKTREE_PATH" || \
  warning "Label sync had issues but continuing..."

echo ""

# ============================================================================
# STEP 4b: Build MCP Server and Generate Configuration
# ============================================================================
CURRENT_STEP="MCP Setup"
header "Step 4b: MCP Server Setup"
echo ""

info "Building unified MCP server..."
if "$LOOM_ROOT/scripts/setup-mcp.sh"; then
  success "MCP server configured"
else
  warning "MCP setup failed - MCP tools will not be available"
  info "Run manually later: $LOOM_ROOT/scripts/setup-mcp.sh"
fi

echo ""

# ============================================================================
# STEP 5: Configure Branch Rulesets
# ============================================================================
CURRENT_STEP="Configure Branch Rulesets"
header "Step 5: Configure Branch Rulesets"
echo ""

# Detect default branch
cd "$TARGET_PATH"
DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "main")
info "Detected default branch: ${DEFAULT_BRANCH}"

# Prompt user
if [[ "$NON_INTERACTIVE" == "true" ]]; then
  info "Non-interactive mode: Skipping branch ruleset setup"
  info "To configure manually, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
else
  echo ""
  read -p "Configure branch ruleset for '${DEFAULT_BRANCH}' branch? (y/N) " -n 1 -r
  echo ""
  if [[ $REPLY =~ ^[Yy]$ ]]; then
    info "Applying branch ruleset..."

    # Apply ruleset
    if "$LOOM_ROOT/scripts/install/setup-branch-protection.sh" "$TARGET_PATH" "$DEFAULT_BRANCH"; then
      echo ""
    else
      echo ""
      warning "Failed to configure branch ruleset (may require admin permissions)"
      info "You can configure manually via GitHub Settings > Rules > Rulesets"
    fi
  else
    info "Skipping branch ruleset setup"
    info "To configure later, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
  fi
fi

echo ""

# ============================================================================
# STEP 5b: Configure Repository Settings
# ============================================================================
CURRENT_STEP="Configure Repository Settings"
header "Step 5b: Configure Repository Settings"
echo ""

if [[ "$NON_INTERACTIVE" == "true" ]]; then
  # In non-interactive mode, apply repository settings automatically
  # This is needed for auto-merge to work on the installation PR
  info "Applying repository settings (required for auto-merge)..."
  if "$LOOM_ROOT/scripts/install/setup-repository-settings.sh" "$TARGET_PATH"; then
    echo ""
  else
    echo ""
    warning "Failed to configure repository settings (may require admin permissions)"
    info "Auto-merge may not be available for the installation PR"
  fi
else
  echo ""
  read -p "Configure repository merge and auto-merge settings? (y/N) " -n 1 -r
  echo ""
  if [[ $REPLY =~ ^[Yy]$ ]]; then
    info "Applying repository settings..."
    if "$LOOM_ROOT/scripts/install/setup-repository-settings.sh" "$TARGET_PATH"; then
      echo ""
    else
      echo ""
      warning "Failed to configure repository settings (may require admin permissions)"
      info "You can configure manually via GitHub Settings > General"
    fi
  else
    info "Skipping repository settings"
    info "To configure later, run: $LOOM_ROOT/scripts/install/setup-repository-settings.sh $TARGET_PATH"
  fi
fi

echo ""

# ============================================================================
# STEP 6: Create Pull Request
# ============================================================================
CURRENT_STEP="Create PR"
header "Step 6: Creating Pull Request"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-pr.sh" ]]; then
  error "Installation script not found: create-pr.sh"
fi

PR_URL_RAW=$("$LOOM_ROOT/scripts/install/create-pr.sh" "$TARGET_PATH/$WORKTREE_PATH" "$BASE_BRANCH") || {
  # create-pr.sh prints detailed error info to stderr (visible above)
  error "Failed to create pull request (see details above)"
}

# Check if installation was already complete (no changes needed)
# IMPORTANT: Check this BEFORE trying to parse as URL
if [[ "$PR_URL_RAW" == *"NO_CHANGES_NEEDED"* ]]; then
  info "Loom is already installed - cleaning up..."

  # Remove the worktree
  cd "$TARGET_PATH"
  git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
  git branch -D "${BRANCH_NAME}" 2>/dev/null || true

  # Restore any staged changes left by --clean uninstall
  # When --clean runs uninstall in --local mode, it stages file deletions
  # Since no changes are needed, we restore those files to their original state
  if [[ "$CLEAN_FIRST" == "true" ]]; then
    info "Restoring files staged by uninstall..."
    if ! git -C "$TARGET_PATH" restore --staged . 2>/dev/null; then
      warning "Failed to unstage changes - run 'git restore --staged .' in $TARGET_PATH"
    fi
    if ! git -C "$TARGET_PATH" checkout -- . 2>/dev/null; then
      warning "Failed to restore files - run 'git checkout -- .' in $TARGET_PATH"
    fi

    # Verify cleanup
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      warning "Working tree still has changes after cleanup:"
      git -C "$TARGET_PATH" status --short 2>/dev/null || true
      warning "To fix: cd $TARGET_PATH && git restore --staged . && git checkout -- ."
    fi
  fi

  # Disable error trap and exit successfully
  trap - EXIT SIGINT SIGTERM

  echo ""
  success "Loom is already installed in this repository"
  echo ""
  info "No pull request was created because all files are up to date."
  echo ""
  exit 0
fi

# Parse output: PR_URL|MERGE_STATUS
# Extract the last line containing PR_URL|STATUS format
LAST_OUTPUT_LINE=$(echo "$PR_URL_RAW" | tail -1)
PR_URL=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f1)
MERGE_STATUS=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f2)

# Validate PR URL - also try to extract from the raw output if parsing failed
if [[ ! "$PR_URL" =~ ^https://github\.com/ ]]; then
  # Fallback: try to extract URL from anywhere in the output
  PR_URL=$(echo "$PR_URL_RAW" | grep -oE 'https://github\.com/[^[:space:]|]+/pull/[0-9]+' | head -1 | tr -d '[:space:]')
fi

if [[ ! "$PR_URL" =~ ^https:// ]]; then
  error "Invalid PR URL returned: $PR_URL"
fi

# Default merge status if not set
MERGE_STATUS="${MERGE_STATUS:-manual}"

success "Pull request created"
echo ""

# ============================================================================
# Post-Install Verification: Ensure main working tree is clean
# ============================================================================
CURRENT_STEP="Verify Working Tree"
header "Verifying main working directory..."
echo ""

cd "$TARGET_PATH"
VERIFY_CLEAN=true

# Check for staged changes
if ! git diff --staged --quiet 2>/dev/null; then
  VERIFY_CLEAN=false
  warning "Main working directory has staged changes after installation"
fi

# Check for unstaged changes
if ! git diff --quiet 2>/dev/null; then
  VERIFY_CLEAN=false
  warning "Main working directory has unstaged changes after installation"
fi

if [[ "$VERIFY_CLEAN" == "true" ]]; then
  success "Main working directory is clean"
else
  echo ""
  warning "Unexpected changes detected in main working directory:"
  git status --short 2>/dev/null || true
  echo ""
  warning "To clean up manually:"
  warning "  cd $TARGET_PATH"
  warning "  git restore --staged ."
  warning "  git checkout -- ."
fi

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

# Show PR status based on merge result
case "$MERGE_STATUS" in
  merged)
    info "✓ Pull Request: ${PR_URL} (merged)"
    ;;
  auto)
    info "⏳ Pull Request: ${PR_URL} (auto-merge enabled)"
    ;;
  *)
    info "📦 Pull Request: ${PR_URL}"
    ;;
esac
echo ""

header "What's Included:"
echo "  ✅ .loom/ directory with configuration and scripts"
echo "  ✅ .claude/ directory with slash commands"
echo "  ✅ .github/ directory with labels and workflows"
echo "  ✅ CLAUDE.md documentation"
echo ""

header "Next Steps:"
case "$MERGE_STATUS" in
  merged)
    # PR was merged - ready to use immediately
    echo "  Loom is ready to use! Choose your workflow:"
    echo ""
    echo "  Manual Mode (recommended to start):"
    echo "    cd $TARGET_PATH && claude"
    echo "    Then use /builder, /judge, or other role commands"
    echo ""
    echo "  Tauri App Mode (requires Loom.app - see README):"
    echo "    Download Loom.app from releases, open workspace"
    ;;
  auto)
    # Auto-merge enabled - PR will merge once requirements are met
    echo "  The installation PR has auto-merge enabled and will merge"
    echo "  automatically once ruleset requirements are met."
    echo ""
    echo "  Once merged, choose your workflow:"
    echo ""
    echo "  Manual Mode (recommended to start):"
    echo "    cd $TARGET_PATH && claude"
    echo "    Then use /builder, /judge, or other role commands"
    echo ""
    echo "  Tauri App Mode (requires Loom.app - see README):"
    echo "    Download Loom.app from releases, open workspace"
    ;;
  *)
    # Manual merge required
    echo "  1. Review and merge the pull request: ${PR_URL}"
    echo "  2. Choose your workflow:"
    echo "     Manual Mode (recommended to start):"
    echo "       cd $TARGET_PATH && claude"
    echo "       Then use /builder, /judge, or other role commands"
    echo "     Tauri App Mode (requires Loom.app - see README):"
    echo "       Download Loom.app from releases, open workspace"
    ;;
esac
echo ""

info "See CLAUDE.md in the target repository for complete usage details."
echo ""
