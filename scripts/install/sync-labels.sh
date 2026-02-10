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
  if output=$(gh label delete "$label" -R "$REPO" --yes 2>&1); then
    info "Deleted default label: $label"
  elif ! echo "$output" | grep -qi "not found\|404"; then
    # Only warn if it failed for a reason other than "not found" or 404
    warning "Could not delete label '$label': $output"
  fi
done

# Sync Loom workflow labels
# This will:
# - Create missing labels
# - Update existing labels with new descriptions/colors
info "Syncing Loom workflow labels..."

# Parse YAML file and sync labels
# YAML format: - name: label-name\n  description: desc\n  color: "HEXCODE"
label_count=0
while IFS= read -u 3 -r line; do
  # Extract label name
  if [[ "$line" =~ ^-\ name:\ (.+)$ ]]; then
    name="${BASH_REMATCH[1]}"
    # Read next two lines for description and color
    read -u 3 -r desc_line
    read -u 3 -r color_line

    # Extract description and color
    if [[ "$desc_line" =~ description:\ (.+)$ ]]; then
      description="${BASH_REMATCH[1]}"
      # Remove quotes if present
      description="${description//\"/}"
    fi

    if [[ "$color_line" =~ color:\ \"?([0-9A-Fa-f]{6})\"?.*$ ]]; then
      color="${BASH_REMATCH[1]}"
    fi

    # Try to create or update the label
    # Use gh label list to check existence, suppressing grep's stderr only
    if gh label list -R "$REPO" --json name --jq '.[].name' 2>&1 | grep -q "^${name}$" 2>/dev/null; then
      # Label exists, update it
      if output=$(gh label edit "$name" -R "$REPO" --description "$description" --color "$color" 2>&1); then
        info "Updated label: $name"
      else
        warning "Failed to update label: $name"
        echo "$output" >&2
      fi
    else
      # Label doesn't exist, create it
      if output=$(gh label create "$name" -R "$REPO" --description "$description" --color "$color" 2>&1); then
        info "Created label: $name"
      else
        # Check if it failed because the label already exists
        if echo "$output" | grep -q "already exists"; then
          info "Label '$name' already exists, attempting update instead..."
          if update_output=$(gh label edit "$name" -R "$REPO" --description "$description" --color "$color" 2>&1); then
            info "Updated label: $name"
          else
            warning "Failed to update label: $name"
            echo "$update_output" >&2
          fi
        else
          warning "Failed to create label: $name"
          echo "$output" >&2
        fi
      fi
    fi

    ((label_count++)) || true
  fi
done 3< "$LABELS_FILE"

if [ "$label_count" -gt 0 ]; then
  success "Synced $label_count labels"
else
  warning "No labels found in $LABELS_FILE"
fi
