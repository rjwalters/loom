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

# Remove default GitHub labels that clutter issue tracking
DEFAULT_LABELS=(
  "bug"
  "documentation"
  "duplicate"
  "enhancement"
  "good first issue"
  "help wanted"
  "invalid"
  "question"
  "wontfix"
)

info "Removing default GitHub labels..."
for label in "${DEFAULT_LABELS[@]}"; do
  if gh label delete "$label" --yes 2>/dev/null; then
    info "Deleted default label: $label"
  fi
done

# Sync Loom workflow labels
# This will:
# - Create missing labels
# - Update existing labels with new descriptions/colors
info "Syncing Loom workflow labels..."
if gh label sync --file "$LABELS_FILE" --force; then
  success "GitHub labels synced"
else
  error "Label sync failed"
  error "Please sync labels manually: gh label sync --file $LABELS_FILE --force"
  exit 1
fi
