#!/usr/bin/env bash
# Sync workflow labels from .github/labels.yml
#
# Supports both GitHub (via gh CLI) and Gitea (via API).
#
# Additive by default (#5066): creates/updates the Loom labels from
# .github/labels.yml and never touches GitHub's default labels (bug,
# documentation, duplicate, enhancement, good first issue, help wanted,
# invalid, question, wontfix) unless --prune-defaults is passed. Deleting
# those is destructive to pre-existing repo data — it silently strips the
# label from every issue/PR carrying it. A default label still in use is
# skipped (with a warning naming the affected issue/PR numbers) unless
# --force is also given.
#
# Usage:
#   sync-labels.sh [--prune-defaults] [--force] [--] [WORKTREE_PATH]
#
# This is the source-only counterpart of the installed-tree
# defaults/scripts/sync-labels.sh (which additionally supports --repo and
# --dry-run). Keep --prune-defaults/--force behavior in parity between the
# two when editing either.

set -euo pipefail

WORKTREE_PATH="."
PRUNE_DEFAULTS=0
FORCE_PRUNE=0
POSITIONAL_SEEN=0
POSITIONAL_ONLY=0

take_positional() {
  if [[ "$POSITIONAL_SEEN" -eq 1 ]]; then
    echo "Unexpected extra argument: $1" >&2
    exit 2
  fi
  WORKTREE_PATH="$1"
  POSITIONAL_SEEN=1
}

while [[ $# -gt 0 ]]; do
  if [[ "$POSITIONAL_ONLY" -eq 1 ]]; then
    take_positional "$1"
    shift
    continue
  fi
  case "$1" in
    --prune-defaults)
      PRUNE_DEFAULTS=1
      shift
      ;;
    --force)
      FORCE_PRUNE=1
      shift
      ;;
    --)
      POSITIONAL_ONLY=1
      shift
      ;;
    -*)
      echo "Unknown option: $1" >&2
      exit 2
      ;;
    *)
      take_positional "$1"
      shift
      ;;
  esac
done

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

# Numbers (one per line) of open+closed issues/PRs currently carrying
# `label`. GitHub's REST /issues endpoint returns both issues and pull
# requests, so one query covers both. Capped at 100 results (max page size).
# Best-effort: any API failure is treated as "no known usage" rather than
# blocking the caller.
github_label_usage() {
  local label="$1"
  gh api "repos/${REPO}/issues" \
    -f state=all -f per_page=100 -f "labels=${label}" \
    --jq '.[].number' 2>/dev/null || true
}

# Format a newline-separated list of issue/PR numbers as "#1 #2 #3", noting
# when the list was truncated at github_label_usage's 100-result cap.
join_usage_numbers() {
  local numbers="$1" count list
  count=$(printf '%s\n' "$numbers" | grep -c . || true)
  list=$(printf '%s\n' "$numbers" | sed '/^$/d; s/^/#/' | paste -sd' ' -)
  if [[ "$count" -ge 100 ]]; then
    printf '%s (100+ shown, there may be more)' "$list"
  else
    printf '%s' "$list"
  fi
}

# Delete a default label unless it is currently in use, in which case skip it
# (warn) or delete it anyway (warn, then delete) depending on FORCE_PRUNE.
github_maybe_delete_label() {
  local label="$1"
  local usage_numbers usage_list
  usage_numbers="$(github_label_usage "$label")"
  if [[ -n "$usage_numbers" ]]; then
    usage_list="$(join_usage_numbers "$usage_numbers")"
    if [[ "$FORCE_PRUNE" -eq 1 ]]; then
      warning "Deleting in-use default label '$label' (--force): affects issue/PR $usage_list"
      github_delete_label "$label"
    else
      warning "Refusing to delete in-use default label '$label': affects issue/PR $usage_list (pass --force to delete anyway)"
    fi
  else
    github_delete_label "$label"
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

# Numbers (one per line) of open+closed issues/PRs currently carrying
# `label`. Gitea's issues endpoint filters by numeric label ID (not name)
# and, with type=all, includes both issues and pull requests. Best-effort:
# any lookup failure reports "no known usage" rather than blocking the
# caller.
gitea_label_usage() {
  local label="$1"
  local response body http_code label_id

  response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}/labels")
  http_code=$(echo "$response" | tail -1)
  body=$(echo "$response" | sed '$d')
  [[ "$http_code" == "200" ]] || return 0

  label_id=$(echo "$body" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for l in data:
    if l['name'] == '$label':
        print(l['id'])
        break
" 2>/dev/null || echo "")
  [[ -n "$label_id" ]] || return 0

  response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}/issues?labels=${label_id}&state=all&type=all")
  http_code=$(echo "$response" | tail -1)
  body=$(echo "$response" | sed '$d')
  [[ "$http_code" == "200" ]] || return 0

  echo "$body" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for item in data:
    n = item.get('number')
    if n is not None:
        print(n)
" 2>/dev/null || true
}

# Delete a default label unless it is currently in use, in which case skip it
# (warn) or delete it anyway (warn, then delete) depending on FORCE_PRUNE.
gitea_maybe_delete_label() {
  local label="$1"
  local usage_numbers usage_list
  usage_numbers="$(gitea_label_usage "$label")"
  if [[ -n "$usage_numbers" ]]; then
    usage_list="$(join_usage_numbers "$usage_numbers")"
    if [[ "$FORCE_PRUNE" -eq 1 ]]; then
      warning "Deleting in-use default label '$label' (--force): affects issue/PR $usage_list"
      gitea_delete_label "$label"
    else
      warning "Refusing to delete in-use default label '$label': affects issue/PR $usage_list (pass --force to delete anyway)"
    fi
  else
    gitea_delete_label "$label"
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

# Additive by default (#5066): deleting GitHub's default labels is
# destructive to pre-existing repo data (it strips the label from every
# issue/PR carrying it), so it only happens with the explicit --prune-defaults
# opt-in.
if [[ "$PRUNE_DEFAULTS" -eq 1 ]]; then
  info "Pruning default labels (--prune-defaults)..."
  for label in "${DEFAULT_LABELS[@]}"; do
    if [[ "$FORGE_TYPE" == "github" ]]; then
      github_maybe_delete_label "$label"
    elif [[ "$FORGE_TYPE" == "gitea" ]]; then
      gitea_maybe_delete_label "$label"
    fi
  done
else
  info "Leaving default labels untouched (pass --prune-defaults to remove them): ${DEFAULT_LABELS[*]}"
fi

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
