#!/usr/bin/env bash
# Wrapper script to install Loom into a target repository
# Usage: ./scripts/install-loom.sh /path/to/target-repo

set -euo pipefail

# Determine Loom repository root (where this script lives)
LOOM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Target repository path (required argument)
TARGET_PATH="${1:-}"

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
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

# Validate arguments
if [[ -z "$TARGET_PATH" ]]; then
  error "Target repository path required\nUsage: $0 /path/to/target-repo"
fi

# Resolve target to absolute path
TARGET_PATH="$(cd "$TARGET_PATH" && pwd 2>/dev/null)" || \
  error "Target path does not exist: $1"

info "Installing Loom into: $TARGET_PATH"

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

# Export environment variables for Claude Code slash command
export LOOM_VERSION
export LOOM_COMMIT
export LOOM_ROOT
export TARGET_PATH

info "Launching Claude Code with /install-loom command..."
echo ""

# Launch Claude Code in target repository with slash command
cd "$TARGET_PATH"

# Check if claude CLI is available
if ! command -v claude &> /dev/null; then
  error "Claude Code CLI (claude) is not installed or not in PATH"
fi

# Launch Claude Code with the install-loom slash command
exec claude code "/install-loom"
