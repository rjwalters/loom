#!/usr/bin/env bash
# Sync GitHub labels from .github/labels.yml

set -euo pipefail

WORKTREE_PATH="${1:-.}"

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

info() {
  echo -e "${BLUE}ℹ $*${NC}" >&2
}

success() {
  echo -e "${GREEN}✓ $*${NC}" >&2
}

warning() {
  echo -e "${YELLOW}⚠ Warning: $*${NC}" >&2
}

cd "$WORKTREE_PATH"

LABELS_FILE=".github/labels.yml"

if [[ ! -f "$LABELS_FILE" ]]; then
  warning "Labels file not found: $LABELS_FILE"
  warning "Skipping label sync"
  exit 0
fi

info "Syncing GitHub labels from $LABELS_FILE..."

# Sync labels using gh CLI
# This will:
# - Create missing labels
# - Update existing labels with new descriptions/colors
# - NOT delete labels that aren't in the file (safe for existing repos)
if gh label sync --file "$LABELS_FILE" --dry-run; then
  info "Label sync dry-run successful, applying changes..."
  gh label sync --file "$LABELS_FILE"
  success "GitHub labels synced"
else
  warning "Label sync failed, continuing anyway"
  warning "You may need to sync labels manually: gh label sync --file $LABELS_FILE"
fi
