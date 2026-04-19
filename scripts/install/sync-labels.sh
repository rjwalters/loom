#!/usr/bin/env bash
# Sync workflow labels from .github/labels.yml
#
# Supports both GitHub (via gh CLI) and Gitea (via API).

set -euo pipefail

WORKTREE_PATH="${1:-.}"

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

LABELS_FILE=".github/labels.yml"

if [[ ! -f "$LABELS_FILE" ]]; then
  warning "Labels file not found: $LABELS_FILE"
  warning "Skipping label sync"
  exit 0
fi

info "Syncing workflow labels from $LABELS_FILE..."

# ============================================================================
# GitHub label operations
# ============================================================================

github_delete_label() {
  local label="$1"
  if output=$(gh label delete "$label" -R "$REPO" --yes 2>&1); then
    info "Deleted default label: $label"
  elif ! echo "$output" | grep -qi "not found\|404"; then
    warning "Could not delete label '$label': $output"
  fi
}

github_sync_label() {
  local name="$1" description="$2" color="$3"

  if gh label list -R "$REPO" --json name --jq '.[].name' 2>&1 | grep -q "^${name}$" 2>/dev/null; then
    if output=$(gh label edit "$name" -R "$REPO" --description "$description" --color "$color" 2>&1); then
      info "Updated label: $name"
    else
      warning "Failed to update label: $name"
      echo "$output" >&2
    fi
  else
    if output=$(gh label create "$name" -R "$REPO" --description "$description" --color "$color" 2>&1); then
      info "Created label: $name"
    else
      if echo "$output" | grep -q "already exists"; then
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
}

# ============================================================================
# Gitea label operations
# ============================================================================

gitea_delete_label() {
  local label="$1"

  # Find label ID by name
  local response body http_code label_id
  response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels")
  http_code=$(echo "$response" | tail -1)
  body=$(echo "$response" | sed '$d')

  if [[ "$http_code" != "200" ]]; then
    warning "Could not list labels to delete '$label'"
    return
  fi

  label_id=$(echo "$body" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for l in data:
    if l['name'] == '$label':
        print(l['id'])
        break
" 2>/dev/null || echo "")

  if [[ -n "$label_id" ]]; then
    local del_response del_code
    del_response=$(gitea_api DELETE "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels/${label_id}")
    del_code=$(echo "$del_response" | tail -1)
    if [[ "$del_code" == "204" ]]; then
      info "Deleted default label: $label"
    else
      warning "Could not delete label '$label' (HTTP $del_code)"
    fi
  fi
}

gitea_sync_label() {
  local name="$1" description="$2" color="$3"

  # Check if label exists
  local response body http_code label_id
  response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels")
  http_code=$(echo "$response" | tail -1)
  body=$(echo "$response" | sed '$d')

  label_id=""
  if [[ "$http_code" == "200" ]]; then
    label_id=$(echo "$body" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for l in data:
    if l['name'] == $(python3 -c "import json; print(json.dumps('$name'))"):
        print(l['id'])
        break
" 2>/dev/null || echo "")
  fi

  local payload
  payload="{\"name\":$(echo "$name" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))'),\"description\":$(echo "$description" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().strip()))'),\"color\":\"#${color}\"}"

  if [[ -n "$label_id" ]]; then
    # Update existing label
    local update_response update_code
    update_response=$(gitea_api PATCH "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels/${label_id}" "$payload")
    update_code=$(echo "$update_response" | tail -1)
    if [[ "$update_code" == "200" ]]; then
      info "Updated label: $name"
    else
      warning "Failed to update label: $name (HTTP $update_code)"
    fi
  else
    # Create new label
    local create_response create_code
    create_response=$(gitea_api POST "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels" "$payload")
    create_code=$(echo "$create_response" | tail -1)
    if [[ "$create_code" == "201" ]]; then
      info "Created label: $name"
    else
      warning "Failed to create label: $name (HTTP $create_code)"
    fi
  fi
}

# ============================================================================
# Main sync logic
# ============================================================================

# Remove default labels that clutter issue tracking
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

info "Removing default labels..."
for label in "${DEFAULT_LABELS[@]}"; do
  if [[ "$FORGE_TYPE" == "github" ]]; then
    github_delete_label "$label"
  elif [[ "$FORGE_TYPE" == "gitea" ]]; then
    gitea_delete_label "$label"
  fi
done

# Sync Loom workflow labels
info "Syncing Loom workflow labels..."

label_count=0
while IFS= read -u 3 -r line; do
  if [[ "$line" =~ ^-\ name:\ (.+)$ ]]; then
    name="${BASH_REMATCH[1]}"
    read -u 3 -r desc_line
    read -u 3 -r color_line

    description=""
    color=""

    if [[ "$desc_line" =~ description:\ (.+)$ ]]; then
      description="${BASH_REMATCH[1]}"
      description="${description//\"/}"
    fi

    if [[ "$color_line" =~ color:\ \"?([0-9A-Fa-f]{6})\"?.*$ ]]; then
      color="${BASH_REMATCH[1]}"
    fi

    if [[ "$FORGE_TYPE" == "github" ]]; then
      github_sync_label "$name" "$description" "$color"
    elif [[ "$FORGE_TYPE" == "gitea" ]]; then
      gitea_sync_label "$name" "$description" "$color"
    fi

    ((label_count++)) || true
  fi
done 3< "$LABELS_FILE"

if [ "$label_count" -gt 0 ]; then
  success "Synced $label_count labels"
else
  warning "No labels found in $LABELS_FILE"
fi
