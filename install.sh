#!/usr/bin/env bash
# Loom Setup - Install Loom into a target repository
# Usage: ./setup.sh [/path/to/target-repo]

set -euo pipefail

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
  error "Not a git repository: $TARGET_PATH\n       Run 'git init' first or choose a different directory."
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

# Check if Loom is already installed
if [[ -d "$TARGET_PATH/.loom" ]]; then
  warning "Loom appears to be already installed in this repository"
  echo ""
  read -r -p "Reinstall and overwrite existing configuration? [y/N] " -n 1 REPLY
  echo ""
  if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
  FORCE_FLAG="--force"
else
  FORCE_FLAG=""
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
read -r -p "Proceed with installation? [y/N] " -n 1 PROCEED
echo ""
if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
  info "Installation cancelled"
  exit 0
fi

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

    # Run loom-daemon init
    "$LOOM_ROOT/target/release/loom-daemon" init $FORCE_FLAG "$TARGET_PATH" || \
      error "Installation failed"

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
    exec "$LOOM_ROOT/scripts/install-loom.sh" "$TARGET_PATH"
    ;;
esac

echo ""
header "═══════════════════════════════════════════════════════════"
echo ""
