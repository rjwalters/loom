#!/usr/bin/env bash
# Uninstall Loom from a target repository
#
# AGENT USAGE INSTRUCTIONS:
#   This script removes Loom orchestration from a target Git repository.
#
#   Non-interactive mode (for Claude Code):
#     ./scripts/uninstall-loom.sh --yes /path/to/target-repo
#     ./scripts/uninstall-loom.sh -y /path/to/target-repo
#
#   Interactive mode (prompts for confirmations):
#     ./scripts/uninstall-loom.sh /path/to/target-repo
#
#   Force auto-merge (create PR and auto-merge):
#     ./scripts/uninstall-loom.sh --force /path/to/target-repo
#
#   Local mode (remove files in working directory, no worktree/PR):
#     ./scripts/uninstall-loom.sh --yes --local /path/to/target-repo
#
#   Clean mode (remove all files including unknown, for clean reinstall):
#     ./scripts/uninstall-loom.sh --yes --local --clean /path/to/target-repo
#
#   What this script does:
#     1. Validates target repository (must be a Git repo with Loom installed)
#     2. Creates uninstall worktree (.loom/worktrees/loom-uninstall)
#     3. Removes Loom files (based on defaults/ manifest)
#     4. Smart-removes CLAUDE.md Loom section and .gitignore patterns
#     5. Prompts for unknown files (interactive mode)
#     6. Creates pull request for review
#
#   Requirements:
#     - Target must be a Git repository with Loom installed
#     - GitHub CLI (gh) must be authenticated

set -euo pipefail

# Parse command line arguments
NON_INTERACTIVE=false
FORCE_AUTO_MERGE=false
LOCAL_MODE=false
CLEAN_MODE=false
TARGET_PATH=""

while [[ $# -gt 0 ]]; do
  case $1 in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    -f|--force)
      FORCE_AUTO_MERGE=true
      shift
      ;;
    -l|--local)
      LOCAL_MODE=true
      shift
      ;;
    --clean)
      CLEAN_MODE=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [OPTIONS] /path/to/target-repo"
      echo ""
      echo "Options:"
      echo "  -y, --yes    Non-interactive mode (preserve unknown files)"
      echo "  -f, --force  Auto-merge the uninstall PR after creation"
      echo "  -l, --local  Remove files in working directory (no worktree, no PR)"
      echo "  --clean      Remove all files in managed directories (including unknown files)"
      echo "  -h, --help   Show this help message"
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
    warning "Uninstall failed at step: ${CURRENT_STEP:-unknown}"

    if [[ -n "${WORKTREE_PATH:-}" ]] && [[ -d "${TARGET_PATH}/${WORKTREE_PATH}" ]]; then
      info "Cleaning up worktree: ${WORKTREE_PATH}..."
      cd "$TARGET_PATH" 2>/dev/null || true
      git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
      if [[ -n "${BRANCH_NAME:-}" ]]; then
        git branch -D "${BRANCH_NAME}" 2>/dev/null || true
      fi
    fi

    echo ""
    error "Uninstall did not complete. See above for details."
  fi
}

trap cleanup_on_error EXIT
trap 'exit 130' SIGINT
trap 'exit 143' SIGTERM

# Validate arguments
if [[ -z "$TARGET_PATH" ]]; then
  error "Target repository path required\nUsage: $0 [--yes|-y] [--force|-f] /path/to/target-repo"
fi

# Export for sub-scripts
export NON_INTERACTIVE
export FORCE_AUTO_MERGE
export LOCAL_MODE

# Resolve target to absolute path
TARGET_PATH="$(cd "$TARGET_PATH" 2>/dev/null && pwd)" || \
  error "Target path does not exist: $TARGET_PATH"

# If target is inside a worktree, resolve to the main repository root
MAIN_WORKTREE=$(git -C "$TARGET_PATH" worktree list --porcelain 2>/dev/null | head -4 | grep -m1 '^worktree ' | cut -d' ' -f2- || true)
if [[ -n "$MAIN_WORKTREE" ]] && [[ "$TARGET_PATH" != "$MAIN_WORKTREE" ]]; then
  warning "Target path is inside a worktree: $TARGET_PATH"
  info "Resolving to main repository root: $MAIN_WORKTREE"
  TARGET_PATH="$MAIN_WORKTREE"
fi

echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║           Loom Uninstall - Remove from Repository         ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

info "Target: $TARGET_PATH"
echo ""

# ============================================================================
# STEP 1: Validate Target Repository
# ============================================================================
CURRENT_STEP="Validate Target"
header "Step 1: Validating Target Repository"
echo ""

# Check if target is a git repository
if [[ ! -d "$TARGET_PATH/.git" ]]; then
  error "Target is not a git repository: $TARGET_PATH"
fi
success "Git repository detected"

# Check if Loom is installed
if [[ ! -d "$TARGET_PATH/.loom" ]]; then
  error "Loom is not installed in $TARGET_PATH (no .loom directory found)"
fi
success "Loom installation detected"

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
success "Not the Loom source repository"

# Check if gh CLI is available (not needed for local mode)
if [[ "$LOCAL_MODE" != "true" ]]; then
  if ! command -v gh &> /dev/null; then
    error "GitHub CLI (gh) is not installed. Install from: https://cli.github.com/"
  fi
  success "GitHub CLI (gh) available"

  # Check if gh is authenticated
  if ! gh auth status &> /dev/null; then
    error "GitHub CLI is not authenticated. Run: gh auth login"
  fi
  success "GitHub CLI authenticated"
fi

echo ""

# ============================================================================
# STEP 2: Build Removal Manifest
# ============================================================================
CURRENT_STEP="Build Manifest"
header "Step 2: Building File Removal Manifest"
echo ""

# Arrays to track what will be removed
# Note: Use ${arr[@]+"${arr[@]}"} pattern for empty array safety with set -u on bash 3.2
REMOVE_FILES=()        # Files to definitely remove
REMOVE_DIRS=()         # Directories to remove if empty after file removal
UNKNOWN_FILES=()       # Files in Loom directories that don't match known patterns
SMART_REMOVE_FILES=()  # Files needing smart removal (CLAUDE.md, .gitignore)

# 1. Build manifest from defaults/ directory (if available)
DEFAULTS_PATH="$LOOM_ROOT/defaults"
KNOWN_DEFAULTS=()

if [[ -d "$DEFAULTS_PATH" ]]; then
  # Walk defaults/ to find all files that Loom installs
  while IFS= read -r -d '' file; do
    rel_path="${file#$DEFAULTS_PATH/}"

    # Map defaults paths to target repo paths
    case "$rel_path" in
      # roles/ -> .loom/roles/
      roles/*)
        target_file=".loom/$rel_path"
        ;;
      # scripts/ -> .loom/scripts/
      scripts/*)
        target_file=".loom/$rel_path"
        ;;
      # config.json -> .loom/config.json
      config.json)
        target_file=".loom/config.json"
        ;;
      # package.json -> .loom/package.json (if installed there)
      package.json)
        target_file=".loom/package.json"
        ;;
      # .loom-README.md -> .loom/README.md
      .loom-README.md)
        target_file=".loom/README.md"
        ;;
      # README.md from defaults -> skip (this is Loom's own README)
      README.md)
        continue
        ;;
      # .loom/ files -> .loom/ (CLAUDE.md in .loom/)
      .loom/*)
        target_file="$rel_path"
        ;;
      # .DS_Store -> skip
      .DS_Store)
        continue
        ;;
      # All other files map directly (CLAUDE.md, .claude/*, .codex/*, .github/*)
      *)
        target_file="$rel_path"
        ;;
    esac

    KNOWN_DEFAULTS+=("$target_file")

    # Check if the file exists in target and add to removal list
    # CLAUDE.md gets smart removal (step 6), not direct removal
    if [[ "$target_file" == "CLAUDE.md" ]]; then
      continue
    fi

    if [[ -f "$TARGET_PATH/$target_file" ]]; then
      REMOVE_FILES+=("$target_file")
    fi
  done < <(find -L "$DEFAULTS_PATH" -type f -print0 | sort -z)
else
  warning "Loom defaults/ directory not found at $DEFAULTS_PATH"
  info "Using fallback pattern matching for file detection"
fi

# 2. Add generated role .md files (correspond to .json files)
if [[ -d "$TARGET_PATH/.loom/roles" ]]; then
  for json_file in "$TARGET_PATH/.loom/roles/"*.json; do
    [[ -f "$json_file" ]] || continue
    base_name=$(basename "$json_file" .json)
    md_file=".loom/roles/${base_name}.md"
    if [[ -f "$TARGET_PATH/$md_file" ]]; then
      # Add if not already in the list
      if ! printf '%s\n' ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} | grep -q "^${md_file}$" 2>/dev/null; then
        REMOVE_FILES+=("$md_file")
      fi
    fi
  done
fi

# 3. Add runtime artifacts
RUNTIME_ARTIFACTS=(
  ".loom/state.json"
  ".loom/daemon-state.json"
  ".loom/config.json"
  ".loom/stop-daemon"
  ".loom/manifest.json"
  ".loom/loom-source-path"
  ".loom/metrics_state.json"
  ".loom/stuck-config.json"
  ".loom/activity.db"
)

for artifact in "${RUNTIME_ARTIFACTS[@]}"; do
  if [[ -f "$TARGET_PATH/$artifact" ]]; then
    if ! printf '%s\n' ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} | grep -q "^${artifact}$" 2>/dev/null; then
      REMOVE_FILES+=("$artifact")
    fi
  fi
done

# Add archived daemon state files
for f in "$TARGET_PATH/.loom/"*-daemon-state.json; do
  [[ -f "$f" ]] || continue
  rel_f="${f#$TARGET_PATH/}"
  if ! printf '%s\n' ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} | grep -q "^${rel_f}$" 2>/dev/null; then
    REMOVE_FILES+=("$rel_f")
  fi
done

# Add log files
for f in "$TARGET_PATH/.loom/"*.log; do
  [[ -f "$f" ]] || continue
  rel_f="${f#$TARGET_PATH/}"
  if ! printf '%s\n' ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} | grep -q "^${rel_f}$" 2>/dev/null; then
    REMOVE_FILES+=("$rel_f")
  fi
done

# Add socket files
for f in "$TARGET_PATH/.loom/"*.sock; do
  [[ -f "$f" ]] || continue
  rel_f="${f#$TARGET_PATH/}"
  if ! printf '%s\n' ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} | grep -q "^${rel_f}$" 2>/dev/null; then
    REMOVE_FILES+=("$rel_f")
  fi
done

# 4. Add runtime directories (worktrees, progress)
RUNTIME_DIRS=(
  ".loom/worktrees"
  ".loom/progress"
  ".loom/logs"
)

# 5. Mark CLAUDE.md and .gitignore for smart removal
if [[ -f "$TARGET_PATH/CLAUDE.md" ]]; then
  SMART_REMOVE_FILES+=("CLAUDE.md")
fi

# .gitignore needs pattern removal (not full removal)
if [[ -f "$TARGET_PATH/.gitignore" ]]; then
  SMART_REMOVE_FILES+=(".gitignore")
fi

# 6. Detect unknown files in Loom-installed directories
# Only scan directories that the install process creates — NOT runtime dirs like worktrees/
LOOM_DIRS=(".loom/roles" ".loom/scripts" ".loom/docs" ".claude/commands" ".claude/agents")

# Claimed role name prefixes — any file in .loom/roles/ matching these prefixes
# is owned by Loom and should be removed during uninstall. This handles deprecated
# role files from older versions (e.g., builder-complexity.md, hermit-patterns.md)
# without needing to maintain an explicit list of deprecated filenames.
CLAIMED_ROLE_PREFIXES=(
  architect auditor builder champion curator
  doctor driver guide hermit judge
  loom shepherd
)

for loom_dir in "${LOOM_DIRS[@]}"; do
  if [[ ! -d "$TARGET_PATH/$loom_dir" ]]; then
    continue
  fi

  while IFS= read -r -d '' file; do
    rel_file="${file#$TARGET_PATH/}"

    # Check if this file is in our removal list or known defaults
    is_known=false
    for known in ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"} ${KNOWN_DEFAULTS[@]+"${KNOWN_DEFAULTS[@]}"}; do
      if [[ "$rel_file" == "$known" ]]; then
        is_known=true
        break
      fi
    done

    # For .loom/roles/, also check claimed role name prefixes
    # This catches deprecated role files from older Loom versions
    if [[ "$is_known" == "false" ]] && [[ "$loom_dir" == ".loom/roles" ]]; then
      base_name=$(basename "$rel_file")
      for prefix in "${CLAIMED_ROLE_PREFIXES[@]}"; do
        if [[ "$base_name" == "${prefix}"* ]]; then
          is_known=true
          # Add to removal list since it's a claimed Loom file
          REMOVE_FILES+=("$rel_file")
          break
        fi
      done
    fi

    if [[ "$is_known" == "false" ]]; then
      UNKNOWN_FILES+=("$rel_file")
    fi
  done < <(find "$TARGET_PATH/$loom_dir" -type f -print0 2>/dev/null | sort -z)
done

# Also add the CLI wrapper if it exists (new location: .loom/bin/loom)
if [[ -f "$TARGET_PATH/.loom/bin/loom" ]] && [[ -x "$TARGET_PATH/.loom/bin/loom" ]]; then
  REMOVE_FILES+=(".loom/bin/loom")
fi

# Backward compatibility: also check old location (repo root)
if [[ -f "$TARGET_PATH/loom" ]] && [[ -x "$TARGET_PATH/loom" ]]; then
  REMOVE_FILES+=("loom")
fi

# Track directories to check for emptiness after removal
REMOVE_DIRS=(
  ".loom/bin"
  ".loom/roles"
  ".loom/scripts"
  ".loom/scripts/cli"
  ".loom/docs"
  ".loom"
  ".claude/commands"
  ".claude/agents"
  ".claude"
  ".codex"
)

# Report what was found
info "Files to remove: ${#REMOVE_FILES[@]}"
info "Smart-remove files: ${#SMART_REMOVE_FILES[@]} (CLAUDE.md, .gitignore)"
info "Unknown files found: ${#UNKNOWN_FILES[@]}"
echo ""

if [[ ${#REMOVE_FILES[@]} -eq 0 ]] && [[ ${#SMART_REMOVE_FILES[@]} -eq 0 ]]; then
  warning "No Loom files found to remove"
  info "The .loom directory exists but contains no recognized Loom files"
  echo ""
fi

# ============================================================================
# STEP 3: Confirm Removal (Interactive Mode)
# ============================================================================
CURRENT_STEP="Confirm Removal"
header "Step 3: Confirming File Removal"
echo ""

# Show summary of what will be removed
if [[ ${#REMOVE_FILES[@]} -gt 0 ]]; then
  info "The following Loom files will be removed:"
  for f in "${REMOVE_FILES[@]}"; do
    echo "  - $f"
  done
  echo ""
fi

if [[ ${#SMART_REMOVE_FILES[@]} -gt 0 ]]; then
  info "The following files will be partially cleaned:"
  for f in "${SMART_REMOVE_FILES[@]}"; do
    case "$f" in
      CLAUDE.md)
        echo "  - CLAUDE.md (remove Loom section or entire file if Loom-generated)"
        ;;
      .gitignore)
        echo "  - .gitignore (remove Loom-specific patterns only)"
        ;;
    esac
  done
  echo ""
fi

# Handle unknown files
REMOVE_UNKNOWN_FILES=()
PRESERVED_CUSTOM_FILES=()
if [[ ${#UNKNOWN_FILES[@]} -gt 0 ]]; then
  if [[ "$CLEAN_MODE" == "true" ]]; then
    # Clean mode: remove unknown files from Loom-OWNED directories only
    # NEVER remove unknown files from SHARED directories (.claude/commands/, .claude/agents/)
    # because those may contain custom project-specific commands not installed by Loom
    #
    # Shared directories: directories where both Loom and users put files
    # Loom-owned directories: directories that Loom fully manages (.loom/*)
    SHARED_DIR_PREFIXES=(".claude/commands/" ".claude/agents/")

    for f in "${UNKNOWN_FILES[@]}"; do
      is_shared=false
      for shared_prefix in "${SHARED_DIR_PREFIXES[@]}"; do
        if [[ "$f" == "$shared_prefix"* ]]; then
          is_shared=true
          break
        fi
      done

      if [[ "$is_shared" == "true" ]]; then
        # Preserve custom files in shared directories
        PRESERVED_CUSTOM_FILES+=("$f")
      else
        REMOVE_UNKNOWN_FILES+=("$f")
      fi
    done

    # Report what will be removed vs preserved
    if [[ ${#REMOVE_UNKNOWN_FILES[@]} -gt 0 ]]; then
      if [[ ${#REMOVE_UNKNOWN_FILES[@]} -le 20 ]]; then
        info "Unknown files in Loom-owned directories (removing in clean mode):"
        for f in "${REMOVE_UNKNOWN_FILES[@]}"; do
          echo "  - $f (will remove)"
        done
      else
        info "${#REMOVE_UNKNOWN_FILES[@]} unknown files in Loom-owned directories (removing in clean mode)"
      fi
    fi

    if [[ ${#PRESERVED_CUSTOM_FILES[@]} -gt 0 ]]; then
      info "Custom files in shared directories (preserved):"
      for f in "${PRESERVED_CUSTOM_FILES[@]}"; do
        echo "  - $f (preserved - not installed by Loom)"
      done
    fi
  elif [[ "$NON_INTERACTIVE" == "true" ]]; then
    if [[ ${#UNKNOWN_FILES[@]} -le 20 ]]; then
      info "Unknown files in Loom directories (preserved in non-interactive mode):"
      for f in "${UNKNOWN_FILES[@]}"; do
        echo "  - $f (preserved)"
      done
    else
      info "${#UNKNOWN_FILES[@]} unknown files in Loom directories (preserved in non-interactive mode)"
    fi
  else
    info "Found files in Loom directories that are not standard Loom files:"
    echo ""
    for f in "${UNKNOWN_FILES[@]}"; do
      read -r -p "  Remove $f? [y/N] " -n 1 REMOVE_REPLY
      echo ""
      if [[ $REMOVE_REPLY =~ ^[Yy]$ ]]; then
        REMOVE_UNKNOWN_FILES+=("$f")
      fi
    done
  fi
  echo ""
fi

# Final confirmation in interactive mode
if [[ "$NON_INTERACTIVE" != "true" ]]; then
  TOTAL_REMOVALS=$(( ${#REMOVE_FILES[@]} + ${#REMOVE_UNKNOWN_FILES[@]} + ${#SMART_REMOVE_FILES[@]} ))
  echo ""
  if [[ "$LOCAL_MODE" == "true" ]]; then
    warning "This will modify $TOTAL_REMOVALS files in the working directory."
  else
    warning "This will modify $TOTAL_REMOVALS files in a new branch and create a PR."
  fi
  read -r -p "Proceed with uninstall? [y/N] " -n 1 PROCEED
  echo ""
  if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
    info "Uninstall cancelled"
    trap - EXIT SIGINT SIGTERM
    exit 0
  fi
fi

echo ""

# ============================================================================
# STEP 4: Create Uninstall Worktree (skipped in local mode)
# ============================================================================
if [[ "$LOCAL_MODE" != "true" ]]; then
  CURRENT_STEP="Create Worktree"
  header "Step 4: Creating Uninstall Worktree"
  echo ""

  # Reuse the create-worktree.sh script, but override the branch name
  # We need to temporarily override the default branch name
  cd "$TARGET_PATH"

  # Ensure .loom/worktrees directory exists
  mkdir -p .loom/worktrees

  WORKTREE_PATH=".loom/worktrees/loom-uninstall"
  BASE_BRANCH_NAME="loom/uninstall"

  # Detect the default branch
  DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "")

  if [[ -n "$DEFAULT_BRANCH" ]]; then
    git fetch origin --prune 2>/dev/null || true
    if ! git show-ref --verify --quiet "refs/remotes/origin/${DEFAULT_BRANCH}"; then
      DEFAULT_BRANCH=""
    fi
  fi

  if [[ -z "$DEFAULT_BRANCH" ]]; then
    if git show-ref --verify --quiet refs/remotes/origin/main; then
      DEFAULT_BRANCH="main"
    elif git show-ref --verify --quiet refs/remotes/origin/master; then
      DEFAULT_BRANCH="master"
    fi
  fi

  if [[ -z "$DEFAULT_BRANCH" ]]; then
    if git show-ref --verify --quiet refs/heads/main; then
      DEFAULT_BRANCH="main"
    elif git show-ref --verify --quiet refs/heads/master; then
      DEFAULT_BRANCH="master"
    else
      DEFAULT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    fi
  fi

  # Fetch latest
  info "Fetching latest changes from origin/${DEFAULT_BRANCH}..."
  git fetch origin "${DEFAULT_BRANCH}" 2>/dev/null || true

  # Determine base branch ref
  if git show-ref --verify --quiet "refs/remotes/origin/${DEFAULT_BRANCH}"; then
    BASE_BRANCH="origin/${DEFAULT_BRANCH}"
  elif git show-ref --verify --quiet "refs/heads/${DEFAULT_BRANCH}"; then
    BASE_BRANCH="${DEFAULT_BRANCH}"
  else
    BASE_BRANCH="HEAD"
  fi

  # Clean up any existing worktree
  if [[ -d "$WORKTREE_PATH" ]]; then
    info "Removing existing worktree: $WORKTREE_PATH"
    git worktree remove "$WORKTREE_PATH" --force 2>/dev/null || true
  fi

  # Prune any stale worktree metadata (defensive: handles incomplete cleanup)
  git worktree prune 2>/dev/null || true

  # Find available branch name
  BRANCH_NAME="$BASE_BRANCH_NAME"
  SUFFIX=2

  while true; do
    if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
      git branch -D "$BRANCH_NAME" >/dev/null 2>&1 || true
    fi

    if git worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$BASE_BRANCH" 2>&1; then
      break
    fi

    info "Branch '$BRANCH_NAME' already exists, trying alternative..."
    BRANCH_NAME="${BASE_BRANCH_NAME}-${SUFFIX}"
    SUFFIX=$((SUFFIX + 1))

    if [[ $SUFFIX -gt 10 ]]; then
      error "Could not find available branch name after 10 attempts"
    fi
  done

  success "Worktree created: $WORKTREE_PATH"
  info "Branch: $BRANCH_NAME"
  info "Base: $DEFAULT_BRANCH"
  echo ""
else
  # Local mode: operate directly on working directory
  info "Local mode: operating directly on working directory"
  echo ""
fi

# ============================================================================
# STEP 5: Remove Loom Files
# ============================================================================
CURRENT_STEP="Remove Files"
header "Step 5: Removing Loom Files"
echo ""

if [[ "$LOCAL_MODE" == "true" ]]; then
  WORKTREE_ABS="$TARGET_PATH"
else
  WORKTREE_ABS="$TARGET_PATH/$WORKTREE_PATH"
fi
cd "$WORKTREE_ABS"

REMOVED_COUNT=0
REMOVED_LIST=()

# Remove known Loom files
for file in ${REMOVE_FILES[@]+"${REMOVE_FILES[@]}"}; do
  if [[ -f "$WORKTREE_ABS/$file" ]]; then
    rm -f "$WORKTREE_ABS/$file"
    REMOVED_LIST+=("$file")
    REMOVED_COUNT=$((REMOVED_COUNT + 1))
  fi
done

# Remove user-approved unknown files
for file in ${REMOVE_UNKNOWN_FILES[@]+"${REMOVE_UNKNOWN_FILES[@]}"}; do
  if [[ -f "$WORKTREE_ABS/$file" ]]; then
    rm -f "$WORKTREE_ABS/$file"
    REMOVED_LIST+=("$file")
    REMOVED_COUNT=$((REMOVED_COUNT + 1))
  fi
done

# Remove runtime directories
for dir in "${RUNTIME_DIRS[@]}"; do
  if [[ -d "$WORKTREE_ABS/$dir" ]]; then
    rm -rf "$WORKTREE_ABS/$dir"
    REMOVED_LIST+=("$dir/")
    REMOVED_COUNT=$((REMOVED_COUNT + 1))
  fi
done

# Prune stale worktree metadata from git's internal tracking
# This prevents "missing but already registered worktree" errors on reinstall
info "Pruning git worktree metadata..."
cd "$TARGET_PATH"
git worktree prune 2>/dev/null || true

success "Removed $REMOVED_COUNT files/directories"
echo ""

# ============================================================================
# STEP 6: Smart Remove CLAUDE.md
# ============================================================================
CURRENT_STEP="Smart Remove"
header "Step 6: Smart File Processing"
echo ""

# Handle CLAUDE.md
if [[ -f "$WORKTREE_ABS/CLAUDE.md" ]]; then
  CLAUDE_MD="$WORKTREE_ABS/CLAUDE.md"

  # Check for BEGIN/END markers
  if grep -q '<!-- BEGIN LOOM ORCHESTRATION -->' "$CLAUDE_MD" 2>/dev/null && \
     grep -q '<!-- END LOOM ORCHESTRATION -->' "$CLAUDE_MD" 2>/dev/null; then
    # Remove only the Loom section (between markers, inclusive)
    info "Removing Loom section from CLAUDE.md (marker-based)..."

    # Use sed to remove everything between markers (inclusive)
    sed -i '' '/<!-- BEGIN LOOM ORCHESTRATION -->/,/<!-- END LOOM ORCHESTRATION -->/d' "$CLAUDE_MD"

    # Clean up any trailing blank lines
    sed -i '' -e :a -e '/^\n*$/{$d;N;ba' -e '}' "$CLAUDE_MD"

    # If file is now empty or only whitespace, remove it
    if [[ ! -s "$CLAUDE_MD" ]] || ! grep -q '[^[:space:]]' "$CLAUDE_MD" 2>/dev/null; then
      rm -f "$CLAUDE_MD"
      REMOVED_LIST+=("CLAUDE.md")
      success "CLAUDE.md removed (was entirely Loom content)"
    else
      REMOVED_LIST+=("CLAUDE.md (Loom section removed)")
      success "CLAUDE.md Loom section removed, user content preserved"
    fi
  else
    # No markers - check if the file matches the Loom-generated pattern
    # Look for the characteristic Loom header
    if grep -q '# Loom Orchestration - Repository Guide' "$CLAUDE_MD" 2>/dev/null || \
       grep -q 'Generated by Loom Installation Process' "$CLAUDE_MD" 2>/dev/null; then
      # Entire file appears to be Loom-generated
      rm -f "$CLAUDE_MD"
      REMOVED_LIST+=("CLAUDE.md")
      success "CLAUDE.md removed (Loom-generated)"
    else
      info "CLAUDE.md does not appear to be Loom-generated - preserving"
    fi
  fi
fi

# Handle .gitignore - remove Loom-specific patterns
if [[ -f "$WORKTREE_ABS/.gitignore" ]]; then
  info "Removing Loom patterns from .gitignore..."

  GITIGNORE="$WORKTREE_ABS/.gitignore"

  # Loom-specific patterns to remove (exact matches)
  LOOM_PATTERNS=(
    ".loom/state.json"
    ".loom/worktrees/"
    ".loom/*.log"
    ".loom/*.sock"
    "# Loom - AI Development Orchestration"
  )

  MODIFIED=false
  for pattern in "${LOOM_PATTERNS[@]}"; do
    if grep -qF "$pattern" "$GITIGNORE" 2>/dev/null; then
      # Remove the exact line
      grep -vF "$pattern" "$GITIGNORE" > "${GITIGNORE}.tmp" || true
      mv "${GITIGNORE}.tmp" "$GITIGNORE"
      MODIFIED=true
    fi
  done

  if [[ "$MODIFIED" == "true" ]]; then
    # Clean up consecutive blank lines left by removal
    awk 'NF || prev_blank++ < 1 { print; if (NF) prev_blank=0 }' "$GITIGNORE" > "${GITIGNORE}.tmp"
    mv "${GITIGNORE}.tmp" "$GITIGNORE"

    # Remove trailing blank lines
    sed -i '' -e :a -e '/^\n*$/{$d;N;ba' -e '}' "$GITIGNORE"

    # If file is now empty, remove it
    if [[ ! -s "$GITIGNORE" ]] || ! grep -q '[^[:space:]]' "$GITIGNORE" 2>/dev/null; then
      rm -f "$GITIGNORE"
      REMOVED_LIST+=(".gitignore")
      success ".gitignore removed (was entirely Loom patterns)"
    else
      REMOVED_LIST+=(".gitignore (Loom patterns removed)")
      success ".gitignore Loom patterns removed"
    fi
  else
    info ".gitignore had no Loom patterns to remove"
  fi
fi

echo ""

# ============================================================================
# STEP 7: Clean Up Empty Directories
# ============================================================================
CURRENT_STEP="Clean Directories"
header "Step 7: Cleaning Empty Directories"
echo ""

for dir in "${REMOVE_DIRS[@]}"; do
  dir_path="$WORKTREE_ABS/$dir"
  if [[ -d "$dir_path" ]]; then
    # Check if directory is empty (or only contains .DS_Store)
    remaining=$(find "$dir_path" -type f -not -name '.DS_Store' 2>/dev/null | head -1)
    if [[ -z "$remaining" ]]; then
      rm -rf "$dir_path"
      REMOVED_LIST+=("$dir/ (empty directory)")
      success "Removed empty directory: $dir"
    else
      info "Preserved directory $dir (contains files)"
    fi
  fi
done

echo ""

# ============================================================================
# STEP 8: Create PR (skipped in local mode)
# ============================================================================
if [[ "$LOCAL_MODE" == "true" ]]; then
  # Local mode: stage changes but don't commit or create PR
  CURRENT_STEP="Local Complete"
  header "Step 8: Local Mode Complete"
  echo ""

  cd "$TARGET_PATH"
  git add -A

  if git diff --staged --quiet; then
    info "No changes detected - Loom files may have already been removed"
    trap - EXIT SIGINT SIGTERM
    echo ""
    success "No changes needed - Loom appears to already be removed"
    exit 0
  fi

  # Disable error trap - we completed successfully
  trap - EXIT SIGINT SIGTERM

  echo ""
  success "Loom files removed from working directory"
  info "Changes are staged but not committed (caller will handle commit)"
  echo ""

  info "Removed ${#REMOVED_LIST[@]} items:"
  for item in ${REMOVED_LIST[@]+"${REMOVED_LIST[@]}"}; do
    echo "  - $item"
  done

  if [[ ${#UNKNOWN_FILES[@]} -gt 0 ]]; then
    PRESERVED_COUNT=$(( ${#UNKNOWN_FILES[@]} - ${#REMOVE_UNKNOWN_FILES[@]} ))
    if [[ $PRESERVED_COUNT -gt 0 ]]; then
      echo ""
      info "$PRESERVED_COUNT unknown files were preserved"
    fi
  fi

  echo ""
else
  CURRENT_STEP="Create PR"
  header "Step 8: Creating Pull Request"
  echo ""

  # Build the PR description with removal summary
  REMOVED_SUMMARY=""
  for item in ${REMOVED_LIST[@]+"${REMOVED_LIST[@]}"}; do
    REMOVED_SUMMARY="${REMOVED_SUMMARY}\n- \`${item}\`"
  done

  # Extract version from the target repo's installed Loom if possible
  INSTALLED_VERSION="unknown"
  if [[ -f "$WORKTREE_ABS/.loom/config.json" ]] 2>/dev/null; then
    INSTALLED_VERSION=$(grep -o '"version"[[:space:]]*:[[:space:]]*"[^"]*"' "$WORKTREE_ABS/.loom/config.json" 2>/dev/null | head -1 | sed 's/.*"version"[[:space:]]*:[[:space:]]*"//;s/"//' || echo "unknown")
  fi

  # Set environment variables for create-pr.sh
  export LOOM_VERSION="${INSTALLED_VERSION}"
  export LOOM_COMMIT="uninstall"

  export COMMIT_MSG="Remove Loom orchestration framework

Removes Loom configuration, roles, scripts, and tooling:
- .loom/ directory (configuration, roles, scripts)
- .claude/ slash commands and agent definitions
- .codex/ configuration
- .github/ Loom-specific labels and workflows
- CLAUDE.md documentation
- .gitignore Loom patterns
- Runtime artifacts (state files, logs)"

  export PR_TITLE="Remove Loom orchestration framework"

  # Build the PR body
  PR_BODY_TEXT="## Loom Uninstallation

This PR removes Loom orchestration framework from the repository.

## What's Removed
"

  # Add removed files list
  for item in ${REMOVED_LIST[@]+"${REMOVED_LIST[@]}"}; do
    PR_BODY_TEXT="${PR_BODY_TEXT}
- \`${item}\`"
  done

  PR_BODY_TEXT="${PR_BODY_TEXT}

## Unknown Files
"

  # Add unknown files section
  if [[ ${#UNKNOWN_FILES[@]} -gt 0 ]]; then
    if [[ ${#REMOVE_UNKNOWN_FILES[@]} -gt 0 ]]; then
      PR_BODY_TEXT="${PR_BODY_TEXT}
The following non-standard files were also removed (user-approved):"
      for f in "${REMOVE_UNKNOWN_FILES[@]}"; do
        PR_BODY_TEXT="${PR_BODY_TEXT}
- \`$f\`"
      done
    fi

    PRESERVED_UNKNOWN=()
    for f in "${UNKNOWN_FILES[@]}"; do
      is_removed=false
      for r in ${REMOVE_UNKNOWN_FILES[@]+"${REMOVE_UNKNOWN_FILES[@]}"}; do
        if [[ "$f" == "$r" ]]; then
          is_removed=true
          break
        fi
      done
      if [[ "$is_removed" == "false" ]]; then
        PRESERVED_UNKNOWN+=("$f")
      fi
    done

    if [[ ${#PRESERVED_UNKNOWN[@]} -gt 0 ]]; then
      if [[ ${#PRESERVED_UNKNOWN[@]} -le 20 ]]; then
        PR_BODY_TEXT="${PR_BODY_TEXT}

The following non-standard files were preserved:"
        for f in "${PRESERVED_UNKNOWN[@]}"; do
          PR_BODY_TEXT="${PR_BODY_TEXT}
- \`$f\`"
        done
      else
        PR_BODY_TEXT="${PR_BODY_TEXT}

${#PRESERVED_UNKNOWN[@]} non-standard files were preserved (too many to list individually)."
      fi
    fi
  else
    PR_BODY_TEXT="${PR_BODY_TEXT}
No unknown files detected."
  fi

  PR_BODY_TEXT="${PR_BODY_TEXT}

## Post-Merge Steps

After merging this PR:
1. Optionally remove loom:* labels from the repository
2. Optionally remove branch rulesets added by Loom

---
Generated by [Loom](https://github.com/rjwalters/loom) uninstall"

  export PR_BODY="$PR_BODY_TEXT"

  # Check if there are actual changes to commit
  cd "$WORKTREE_ABS"
  git add -A

  if git diff --staged --quiet; then
    info "No changes detected - Loom files may have already been removed"

    # Clean up worktree
    cd "$TARGET_PATH"
    git worktree remove "$WORKTREE_PATH" --force 2>/dev/null || true
    git branch -D "$BRANCH_NAME" 2>/dev/null || true

    trap - EXIT SIGINT SIGTERM
    echo ""
    success "No changes needed - Loom appears to already be removed"
    exit 0
  fi

  # Use create-pr.sh to handle commit, push, and PR creation
  if [[ -x "$LOOM_ROOT/scripts/install/create-pr.sh" ]]; then
    TARGET_BRANCH="${DEFAULT_BRANCH#origin/}"

    PR_URL_RAW=$("$LOOM_ROOT/scripts/install/create-pr.sh" "$WORKTREE_ABS" "$TARGET_BRANCH") || \
      error "Failed to create pull request"

    # Parse output: PR_URL|MERGE_STATUS
    LAST_OUTPUT_LINE=$(echo "$PR_URL_RAW" | tail -1)
    PR_URL=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f1)
    MERGE_STATUS=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f2)

    # Validate PR URL
    if [[ ! "$PR_URL" =~ ^https://github\.com/ ]]; then
      PR_URL=$(echo "$PR_URL_RAW" | grep -oE 'https://github\.com/[^[:space:]|]+/pull/[0-9]+' | head -1 | tr -d '[:space:]')
    fi

    if [[ ! "$PR_URL" =~ ^https:// ]]; then
      error "Invalid PR URL returned: $PR_URL"
    fi

    MERGE_STATUS="${MERGE_STATUS:-manual}"
  else
    error "create-pr.sh not found at $LOOM_ROOT/scripts/install/create-pr.sh"
  fi

  echo ""

  # ============================================================================
  # Complete
  # ============================================================================
  CURRENT_STEP="Complete"

  # Disable error trap - we completed successfully
  trap - EXIT SIGINT SIGTERM

  echo ""
  header "╔═══════════════════════════════════════════════════════════╗"
  header "║              Loom Uninstall Complete                      ║"
  header "╚═══════════════════════════════════════════════════════════╝"
  echo ""

  case "$MERGE_STATUS" in
    merged)
      success "Loom has been removed from the repository"
      info "Pull request: $PR_URL (merged)"
      ;;
    auto)
      success "Uninstall PR created with auto-merge enabled"
      info "Pull request: $PR_URL (auto-merge enabled)"
      ;;
    *)
      success "Uninstall PR created"
      info "Pull request: $PR_URL"
      echo ""
      info "Review and merge the PR to complete the uninstall."
      ;;
  esac

  echo ""

  info "Removed ${#REMOVED_LIST[@]} items:"
  for item in ${REMOVED_LIST[@]+"${REMOVED_LIST[@]}"}; do
    echo "  - $item"
  done

  if [[ ${#UNKNOWN_FILES[@]} -gt 0 ]]; then
    PRESERVED_COUNT=$(( ${#UNKNOWN_FILES[@]} - ${#REMOVE_UNKNOWN_FILES[@]} ))
    if [[ $PRESERVED_COUNT -gt 0 ]]; then
      echo ""
      info "$PRESERVED_COUNT unknown files were preserved"
    fi
  fi

  echo ""
  header "Post-Merge Steps:"
  echo "  1. Remove loom:* labels (optional):"
  echo "     gh label list | grep 'loom:' | awk '{print \$1}' | xargs -I{} gh label delete {} --yes"
  echo "  2. Remove Loom branch rulesets (optional)"
  echo ""
fi
