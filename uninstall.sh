#!/usr/bin/env bash
# Loom Uninstall - Remove Loom from a target repository
# Usage: ./uninstall.sh [/path/to/target-repo]

set -euo pipefail

# Handle Ctrl-C and SIGTERM during interactive prompts
trap 'echo ""; echo -e "\033[0;34mℹ Uninstall cancelled\033[0m"; exit 130' SIGINT
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
header "║                  Loom Uninstall v1.0                      ║"
header "║       Remove Loom from a Target Repository                ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

# Get target path from argument or prompt
TARGET_PATH="${1:-}"

if [[ -z "$TARGET_PATH" ]]; then
  echo "Enter the path to the repository where Loom is installed:"
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

# Check if Loom is installed
if [[ ! -d "$TARGET_PATH/.loom" ]]; then
  error "Loom is not installed in $TARGET_PATH (no .loom directory found)"
fi

# Check if it's the Loom source repository
is_loom_source_repo() {
  local path="$1"
  [[ -f "$path/.loom-source" ]] && return 0
  [[ -d "$path/src-tauri" && -d "$path/loom-daemon" && -d "$path/defaults" ]] && return 0
  return 1
}

if is_loom_source_repo "$TARGET_PATH"; then
  error "Cannot uninstall Loom from its own source repository"
fi

success "Loom installation detected in $TARGET_PATH"
echo ""

# Check for GitHub CLI
if ! command -v gh &> /dev/null; then
  error "GitHub CLI (gh) is required for uninstallation.\n       Install: brew install gh"
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

# Show what will be removed
header "What Will Be Removed"
echo ""
info "Loom Configuration:"
echo "  - .loom/ directory (roles, scripts, configuration)"
echo "  - .loom/config.json"
echo ""
info "AI Tooling:"
echo "  - .claude/commands/ (slash commands)"
echo "  - .claude/agents/ (agent definitions)"
echo "  - .claude/settings.json"
echo "  - .codex/ (Codex configuration)"
echo ""
info "GitHub Integration:"
echo "  - .github/labels.yml"
echo "  - .github/workflows/label-external-issues.yml"
echo "  - .github/ISSUE_TEMPLATE/"
echo ""
info "Documentation:"
echo "  - CLAUDE.md (Loom section or entire file)"
echo ""
info "Runtime Artifacts:"
echo "  - State files, logs, worktrees"
echo ""
info "Modifications:"
echo "  - .gitignore (Loom patterns removed)"
echo ""
warning "A PR will be created for review before any changes take effect."
echo ""

# Confirm
read -r -p "Proceed with uninstall? [y/N] " -n 1 PROCEED
echo ""
if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
  info "Uninstall cancelled"
  exit 0
fi

echo ""

# Ask about auto-merge
read -r -p "Auto-merge the uninstall PR after creation? [y/N] " -n 1 AUTO_MERGE
echo ""
FORCE_FLAG=""
if [[ $AUTO_MERGE =~ ^[Yy]$ ]]; then
  FORCE_FLAG="--force"
fi

echo ""

# Run uninstall
exec "$LOOM_ROOT/scripts/uninstall-loom.sh" $FORCE_FLAG "$TARGET_PATH"
