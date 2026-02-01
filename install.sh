#!/usr/bin/env bash
# Loom Setup - Install Loom into a target repository
# Usage: ./install.sh [OPTIONS] [/path/to/target-repo]
#
# Options:
#   -y, --yes    Non-interactive mode (skip confirmation prompts)
#   --quick      Quick Install - direct install without GitHub workflow
#   --full       Full Install - creates issue, worktree, and PR
#   -h, --help   Show this help message
#
# Examples:
#   ./install.sh --quick ~/projects/my-app
#   ./install.sh --full /path/to/team-project
#   ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick

set -euo pipefail

# Handle Ctrl-C and SIGTERM during interactive prompts
trap 'echo ""; echo -e "\033[0;34mℹ Installation cancelled\033[0m"; exit 130' SIGINT
trap 'exit 143' SIGTERM

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
  echo -e "${YELLOW}⚠ $*${NC}"
}

header() {
  echo -e "${CYAN}$*${NC}"
}

# Determine Loom repository root
LOOM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Show banner
echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║                    Loom Setup v1.0                        ║"
header "║        AI-Powered Development Orchestration               ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

# Parse flags
NON_INTERACTIVE=false
INSTALL_TYPE=""
while [[ "${1:-}" == -* ]]; do
  case "$1" in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    --quick)
      # Check for conflicting --full flag
      if [[ "$INSTALL_TYPE" == "2" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="1"
      NON_INTERACTIVE=true  # --quick implies non-interactive
      shift
      ;;
    --full)
      # Check for conflicting --quick flag
      if [[ "$INSTALL_TYPE" == "1" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="2"
      NON_INTERACTIVE=true  # --full implies non-interactive
      shift
      ;;
    -h|--help)
      echo "Usage: ./install.sh [OPTIONS] [TARGET_PATH]"
      echo ""
      echo "Options:"
      echo "  -y, --yes    Non-interactive mode (skip confirmation prompts)"
      echo "  --quick      Quick Install - direct install without GitHub workflow"
      echo "  --full       Full Install - creates issue, worktree, and PR"
      echo "  -h, --help   Show this help message"
      echo ""
      echo "Examples:"
      echo "  ./install.sh --quick ~/projects/my-app"
      echo "  ./install.sh --full /path/to/team-project"
      echo "  ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick install"
      exit 0
      ;;
    *)
      error "Unknown flag: $1"
      ;;
  esac
done

# Early validation for --full: requires gh CLI
if [[ "$INSTALL_TYPE" == "2" ]] && ! command -v gh &> /dev/null; then
  error "Full Install requires GitHub CLI (gh)\n       Install: brew install gh\n       Or use --quick for installation without GitHub integration"
fi

# Get target path from argument or prompt
TARGET_PATH="${1:-}"

if [[ -z "$TARGET_PATH" ]]; then
  echo "Enter the path to the repository where you want to install Loom:"
  echo -e "${CYAN}Example: ~/GitHub/my-project or /Users/you/code/my-app${NC}"
  echo ""
  read -r -p "Repository path: " TARGET_PATH
  echo ""
fi

# Expand tilde if present
TARGET_PATH="${TARGET_PATH/#\~/$HOME}"

# Validate target path exists
if [[ ! -d "$TARGET_PATH" ]]; then
  error "Directory does not exist: $TARGET_PATH"
fi

# Resolve to absolute path
TARGET_PATH="$(cd "$TARGET_PATH" && pwd 2>/dev/null)" || \
  error "Cannot access directory: $TARGET_PATH"

info "Target repository: $TARGET_PATH"
echo ""

# Check if it's a git repository
if [[ ! -d "$TARGET_PATH/.git" ]]; then
  warning "$TARGET_PATH is not a git repository."
  echo ""
  echo "Would you like to initialize git and optionally set up GitHub?"
  echo ""
  echo "This will:"
  echo "  1. Run 'git init' in the directory"
  echo "  2. Create a sensible .gitignore file"
  echo "  3. Create an initial commit"
  echo "  4. Optionally create a GitHub repository and set up remote"
  echo ""
  read -r -p "Initialize git repository? [y/N] " -n 1 INIT_GIT
  echo ""

  if [[ ! $INIT_GIT =~ ^[Yy]$ ]]; then
    error "Cannot proceed without a git repository.\n       Run 'git init' manually or choose a different directory."
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

# Loom (will be added by installation)
# .loom/state.json
# .loom/worktrees/
# .loom/*.log
GITIGNORE
    success "Created .gitignore"
  fi

  # Create initial commit
  info "Creating initial commit..."
  git add -A
  git commit -m "Initial commit" --quiet || error "Failed to create initial commit"
  success "Initial commit created"
  echo ""

  # Offer GitHub repository creation
  if command -v gh &> /dev/null; then
    echo "Would you like to create a GitHub repository for this project?"
    echo ""
    read -r -p "Create GitHub repository? [y/N] " -n 1 CREATE_REPO
    echo ""

    if [[ $CREATE_REPO =~ ^[Yy]$ ]]; then
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
        warning "Failed to create GitHub repository. Continuing with local git only."
        info "You can create the repository later with: gh repo create"
      fi
      echo ""
    fi
  else
    info "GitHub CLI (gh) not found - skipping GitHub repository creation"
    info "Install with: brew install gh"
    echo ""
  fi
fi

success "Valid git repository detected"
echo ""

# ============================================================================
# Check Required Dependencies
# ============================================================================
header "Checking System Dependencies"
echo ""

MISSING_DEPS=()
INSTALL_INSTRUCTIONS=""

# Check for Git (should always be present if we got this far, but verify)
if command -v git &> /dev/null; then
  success "git: $(git --version | head -1)"
else
  MISSING_DEPS+=("git")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • git: brew install git"
fi

# Check for Node.js
if command -v node &> /dev/null; then
  success "node: $(node --version)"
else
  MISSING_DEPS+=("node")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • Node.js: brew install node"
fi

# Check for pnpm
if command -v pnpm &> /dev/null; then
  success "pnpm: $(pnpm --version)"
else
  MISSING_DEPS+=("pnpm")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • pnpm: npm install -g pnpm"
fi

# Check for Cargo (Rust toolchain)
if command -v cargo &> /dev/null; then
  success "cargo: $(cargo --version | head -1)"
else
  MISSING_DEPS+=("cargo")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • Rust/Cargo: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# Check for GitHub CLI (optional but needed for Full Install)
if command -v gh &> /dev/null; then
  success "gh: $(gh --version | head -1)"
else
  warning "gh (GitHub CLI) not found - Full Install will not be available"
  info "  Install with: brew install gh"
fi

echo ""

# If any required dependencies are missing, prompt the user
if [[ ${#MISSING_DEPS[@]} -gt 0 ]]; then
  echo ""
  error_no_exit() {
    echo -e "${RED}✗ Missing required dependencies: ${MISSING_DEPS[*]}${NC}"
  }
  error_no_exit

  echo ""
  info "Please install the missing dependencies:"
  echo -e "$INSTALL_INSTRUCTIONS"
  echo ""

  read -r -p "Exit to install dependencies? [Y/n] " -n 1 INSTALL_DEPS
  echo ""
  if [[ ! $INSTALL_DEPS =~ ^[Nn]$ ]]; then
    info "Please install the missing dependencies and run this script again."
    exit 1
  fi

  warning "Continuing without all dependencies may cause build failures"
  echo ""
fi

# Check if this is the Loom source repository (self-installation)
is_loom_source_repo() {
  local path="$1"
  # Check for marker file
  [[ -f "$path/.loom-source" ]] && return 0
  # Check for Loom-specific directory structure
  [[ -d "$path/src-tauri" && -d "$path/loom-daemon" && -d "$path/defaults" ]] && return 0
  return 1
}

if is_loom_source_repo "$TARGET_PATH"; then
  echo ""
  header "╔═══════════════════════════════════════════════════════════╗"
  header "║              Loom Source Repository Detected              ║"
  header "╚═══════════════════════════════════════════════════════════╝"
  echo ""
  info "This appears to be the Loom source repository itself."
  info "Self-installation runs in validation-only mode to prevent data loss."
  echo ""
  info "The Loom repo's .loom/ directory IS the source of truth for defaults."
  info "Installing would overwrite rich content with minimal templates."
  echo ""
  read -r -p "Run validation to check configuration? [Y/n] " -n 1 VALIDATE_REPLY
  echo ""
  if [[ $VALIDATE_REPLY =~ ^[Nn]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
  FORCE_FLAG=""
  SELF_INSTALL=true
elif [[ -d "$TARGET_PATH/.loom" ]]; then
  warning "Loom appears to be already installed in this repository"
  echo ""
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    info "Reinstall will uninstall the existing installation first, then perform"
    info "a fresh Quick Install."
  else
    info "Reinstall will uninstall the existing installation first, then perform"
    info "a fresh install and create a PR with the changes."
  fi
  echo ""

  if [[ "$NON_INTERACTIVE" != true ]]; then
    read -r -p "Proceed with reinstall? [y/N] " -n 1 REINSTALL_CONFIRM
    echo ""
    if [[ ! $REINSTALL_CONFIRM =~ ^[Yy]$ ]]; then
      info "Installation cancelled"
      exit 0
    fi
  else
    info "Non-interactive mode: proceeding with reinstall"
  fi

  # Uninstall existing installation (local mode, no separate PR)
  info "Uninstalling existing Loom installation..."
  "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
    error "Uninstall failed - aborting reinstall"
  echo ""
  success "Existing installation removed"
  echo ""

  # If --quick was specified, do a quick reinstall instead of full workflow
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    info "Running fresh Quick Install..."
    echo ""

    # Check if loom-daemon is built
    if [[ ! -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
      warning "loom-daemon binary not found"
      info "Building loom-daemon (this may take a minute)..."
      cd "$LOOM_ROOT"
      pnpm daemon:build || error "Failed to build loom-daemon"
      echo ""
    fi

    # Run loom-daemon init
    "$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
      error "Installation failed"
    echo ""
    success "Quick reinstallation complete!"
    exit 0
  fi

  # Default: delegate to Full Install (creates worktree + PR)
  info "Running fresh install via Full Install workflow..."
  echo ""
  INSTALL_FLAGS=()
  if [[ "$NON_INTERACTIVE" == true ]]; then
    INSTALL_FLAGS+=(--yes)
  fi
  exec "$LOOM_ROOT/scripts/install-loom.sh" ${INSTALL_FLAGS[@]+"${INSTALL_FLAGS[@]}"} "$TARGET_PATH"
else
  FORCE_FLAG=""
  SELF_INSTALL=false
fi

echo ""
header "What Will Be Installed"
echo ""
info "Configuration (committed to git):"
echo "  • .loom/config.json         - Terminal and role configuration"
echo "  • .loom/roles/*.md          - Agent role definitions (8 roles)"
echo "  • .loom/scripts/            - Helper scripts (worktree.sh, etc.)"
echo ""
info "Documentation (committed to git):"
echo "  • CLAUDE.md                 - AI context for Claude Code (~11KB)"
echo "  • AGENTS.md                 - Agent workflow guide (~12KB)"
echo ""
info "Tooling (committed to git):"
echo "  • .claude/commands/*.md     - Slash commands for Claude Code"
echo "  • .github/labels.yml        - Workflow label definitions"
echo "  • .github/workflows/*.yml   - GitHub Actions (optional)"
echo ""
info "Gitignored (local only):"
echo "  • .loom/state.json          - Runtime terminal state"
echo "  • .loom/worktrees/          - Git worktrees for isolated work"
echo "  • .loom/*.log               - Application logs"
echo ""
warning "Modifications:"
echo "  • .gitignore will be updated with Loom patterns"
echo ""
info "GitHub Changes (if using Full Install):"
echo "  • Creates GitHub labels for workflow coordination"
echo "  • Creates tracking issue with 'loom:building' label"
echo "  • Creates pull request with 'loom:review-requested' label"
echo ""
if [[ "$NON_INTERACTIVE" != true ]]; then
  read -r -p "Proceed with installation? [y/N] " -n 1 PROCEED
  echo ""
  if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
else
  info "Non-interactive mode: proceeding with installation"
fi

# Determine installation method
if [[ -n "$INSTALL_TYPE" ]]; then
  # Installation type was specified via --quick or --full flag
  METHOD="$INSTALL_TYPE"
  if [[ "$METHOD" == "1" ]]; then
    info "Using Quick Install (via --quick flag)"
  else
    info "Using Full Install (via --full flag)"
  fi
elif [[ "$NON_INTERACTIVE" == true ]]; then
  # Non-interactive mode without explicit type defaults to quick install
  METHOD="1"
  info "Non-interactive mode: defaulting to Quick Install"
else
  # Interactive mode: show options and prompt
  echo ""
  header "Installation Options"
  echo ""
  echo "1. Quick Install (Direct)"
  echo "   - Fast installation using loom-daemon init"
  echo "   - No GitHub issue or PR created"
  echo "   - Good for personal projects or quick testing"
  echo ""
  echo "2. Full Install (Workflow)"
  echo "   - Creates GitHub issue to track installation"
  echo "   - Uses git worktree for clean separation"
  echo "   - Syncs labels and creates PR for review"
  echo "   - Recommended for team projects"
  echo ""

  # Retry loop for method selection (up to 3 attempts)
  METHOD=""
  for attempt in 1 2 3; do
    read -r -p "Choose installation method [1/2]: " -n 1 METHOD
    echo ""

    if [[ "$METHOD" == "1" || "$METHOD" == "2" ]]; then
      break
    fi

    if [[ $attempt -lt 3 ]]; then
      warning "Invalid choice '$METHOD'. Please enter 1 or 2."
      echo ""
    else
      error "Invalid choice after 3 attempts. Please run again and select 1 or 2."
    fi
  done
fi

echo ""

case "$METHOD" in
  1)
    info "Running Quick Install..."
    echo ""

    # Check if loom-daemon is built
    if [[ ! -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
      warning "loom-daemon binary not found"
      info "Building loom-daemon (this may take a minute)..."
      cd "$LOOM_ROOT"
      pnpm daemon:build || error "Failed to build loom-daemon"
      echo ""
    fi

    # Handle --clean: run local uninstall first, then fresh install
    if [[ "$FORCE_FLAG" == "--clean" ]]; then
      info "Running local uninstall before fresh install..."
      "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
        error "Uninstall failed - aborting clean install"
      echo ""
      info "Uninstall complete, proceeding with fresh install..."
      "$LOOM_ROOT/target/release/loom-daemon" init --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    else
      # Run loom-daemon init
      "$LOOM_ROOT/target/release/loom-daemon" init $FORCE_FLAG --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    fi

    echo ""
    success "Quick installation complete!"
    ;;

  2)
    info "Running Full Install with Workflow..."
    echo ""

    # Check prerequisites
    if ! command -v gh &> /dev/null; then
      error "GitHub CLI (gh) is required for full installation\n       Install: brew install gh"
    fi

    # Check GitHub authentication
    if ! gh auth status &> /dev/null; then
      warning "GitHub CLI is not authenticated"
      info "Please authenticate with GitHub:"
      echo ""
      gh auth login || error "GitHub authentication failed"
      echo ""
    fi

    success "GitHub CLI is authenticated"
    echo ""

    # Show repository info
    cd "$TARGET_PATH"
    REPO_INFO=$(gh repo view --json nameWithOwner,description 2>/dev/null || echo "{}")
    REPO_NAME=$(echo "$REPO_INFO" | jq -r '.nameWithOwner // "unknown"' 2>/dev/null || echo "unknown")

    if [[ "$REPO_NAME" != "unknown" ]]; then
      info "Target repository: $REPO_NAME"
    else
      warning "Could not detect remote repository. This may be a local-only repo."
      read -r -p "Continue anyway? [y/N] " -n 1 CONTINUE_LOCAL
      echo ""
      if [[ ! $CONTINUE_LOCAL =~ ^[Yy]$ ]]; then
        info "Installation cancelled"
        exit 0
      fi
    fi
    echo ""

    # Run the full installation workflow
    exec "$LOOM_ROOT/scripts/install-loom.sh" $FORCE_FLAG "$TARGET_PATH"
    ;;
esac

echo ""
header "═══════════════════════════════════════════════════════════"
echo ""
