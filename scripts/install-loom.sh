#!/usr/bin/env bash
# Install Loom into a target repository using the full workflow
#
# AGENT USAGE INSTRUCTIONS:
#   This script installs Loom orchestration into a target Git repository.
#
#   Non-interactive mode (for Claude Code):
#     ./scripts/install-loom.sh --yes /path/to/target-repo
#     ./scripts/install-loom.sh -y /path/to/target-repo
#
#   Interactive mode (prompts for confirmations):
#     ./scripts/install-loom.sh /path/to/target-repo
#
#   What this script does:
#     0. Validates source/target checkout state (warns or refuses if either is
#        on a non-main branch or behind origin/main; override with
#        --allow-non-main-source / --allow-stale-target)
#     1. Validates target repository (must be a Git repo)
#     2. Creates installation worktree (.loom/worktrees/loom-install-vX.Y.Z)
#     3. Initializes Loom configuration (copies defaults to .loom/)
#     4. Syncs workflow labels (GitHub or Gitea)
#     5. Configures branch rulesets (interactive mode only)
#     6. Creates pull request with loom:review-requested label
#
#   Requirements:
#     - Target must be a Git repository with a remote
#     - For GitHub: GitHub CLI (gh) must be authenticated
#     - For Gitea: GITEA_TOKEN or FORGE_TOKEN environment variable must be set
#     - loom-daemon binary must be buildable (cargo build --package loom-daemon --release)
#
#   After installation:
#     - Merge the generated PR in the target repository
#     - Loom will be ready to use in that workspace

set -euo pipefail

# Parse command line arguments
NON_INTERACTIVE=false
FORCE_OVERWRITE=false
CLEAN_FIRST=false
ALLOW_ACTIVE_SESSION=false
DOGFOOD_MODE=""  # "" (auto-detect), "true" (forced), "false" (forced off)
ALLOW_NON_MAIN_SOURCE=false
ALLOW_STALE_TARGET=false
SKIP_TARGET_CI=false  # When true, install PR carries `[skip ci]` (issue #3333)
TARGET_PATH=""

while [[ $# -gt 0 ]]; do
  case $1 in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    -f|--force)
      FORCE_OVERWRITE=true
      shift
      ;;
    --clean)
      CLEAN_FIRST=true
      shift
      ;;
    --allow-active-session)
      ALLOW_ACTIVE_SESSION=true
      shift
      ;;
    --dogfood)
      DOGFOOD_MODE="true"
      shift
      ;;
    --no-dogfood)
      DOGFOOD_MODE="false"
      shift
      ;;
    --allow-non-main-source)
      ALLOW_NON_MAIN_SOURCE=true
      shift
      ;;
    --allow-stale-target)
      ALLOW_STALE_TARGET=true
      shift
      ;;
    --skip-target-ci)
      SKIP_TARGET_CI=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [OPTIONS] /path/to/target-repo"
      echo ""
      echo "Options:"
      echo "  -y, --yes                  Non-interactive mode"
      echo "  -f, --force                Force overwrite existing files and enable auto-merge"
      echo "                             (does NOT bypass active-session detection)"
      echo "  --clean                    Run uninstall first, then fresh install (combines both operations)"
      echo "  --allow-active-session     Proceed even if a live Loom session is detected in"
      echo "                             the target (daemon running, recent state file, or"
      echo "                             active issue worktrees). Required to override the"
      echo "                             pre-flight active-session guard."
      echo "  --dogfood                  Force dogfood mode: symlink .claude/agents -> ../defaults/.claude/agents"
      echo "                             (auto-detected when installing into the Loom source repo itself)"
      echo "  --no-dogfood               Disable dogfood mode even when installing into the Loom source repo"
      echo "  --allow-non-main-source    Proceed even if the Loom source checkout is not on a clean main"
      echo "                             (non-main branch, detached HEAD, or behind origin/main)"
      echo "  --allow-stale-target       Proceed even if the target checkout is not on a clean main"
      echo "                             (non-main branch or behind origin/main); ignored in dogfood mode"
      echo "  --skip-target-ci           Add '[skip ci]' to the install PR title and commit subject so the"
      echo "                             target repository's CI does not run on the install PR. Use only when"
      echo "                             the target's required-checks rulesets do not depend on CI completing"
      echo "                             (the universal GitHub-native skip directive)."
      echo "  -h, --help                 Show this help message"
      echo ""
      echo "Default install PR markers (always on — see .loom/docs/ci-integration.md after install):"
      echo "  - PR title prefix:        chore(loom): Install Loom <version>"
      echo "  - PR body marker line:    loom-install: true"
      echo "  - Commit trailer:         Skip-CI-Hint: docs-only"
      echo "  These passive markers are detectable by opt-in CI (path-ignore, title filters,"
      echo "  body grep) and do NOT suppress CI globally. Use --skip-target-ci for the active"
      echo "  global skip when the target's required-checks rulesets do not depend on CI."
      exit 0
      ;;
    *)
      if [[ "$1" == -* ]]; then
        echo "Error: unknown flag: $1" >&2
        echo "       (--quick and --full belong to ./install.sh, not scripts/install-loom.sh)" >&2
        echo "Use --help for usage." >&2
        exit 1
      fi
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

# Source the installed-files manifest helper (#3450). The helper is in its
# own file so the test suite can exercise it without sourcing the full
# installer (which has argv-parsing side effects).
# shellcheck source=scripts/install/manifest.sh
source "$LOOM_ROOT/scripts/install/manifest.sh"

# Check the state of the Loom *source* checkout (the directory that contains
# this script). We refuse to install from a feature branch, a stale main, or
# an arbitrary detached HEAD unless the operator explicitly opts in. Detached
# HEAD on a `v*` tag is treated as a clean tagged release (still emits a mild
# warning so the operator knows they're not on a moving branch). Staleness is
# best-effort: we never run `git fetch` here — the operator is responsible for
# refreshing `origin/main` if they want the staleness check to be accurate.
check_source_state() {
  local branch tag_match=""
  branch=$(git -C "$LOOM_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")

  # Tagged-release exemption: detached HEAD that resolves to a v* tag is OK.
  if [[ "$branch" == "HEAD" ]]; then
    tag_match=$(git -C "$LOOM_ROOT" describe --exact-match --tags HEAD 2>/dev/null || true)
  fi

  local stale=""
  # Best-effort: only check upstream if origin/main exists locally. If there
  # is no origin remote at all, or origin/main has never been fetched, we
  # skip the staleness check silently rather than erroring — the operator
  # has not opted into upstream tracking.
  if git -C "$LOOM_ROOT" rev-parse --verify --quiet origin/main >/dev/null 2>&1; then
    local behind
    behind=$(git -C "$LOOM_ROOT" rev-list --count HEAD..origin/main 2>/dev/null || echo 0)
    if [[ "$behind" -gt 0 ]]; then
      stale="local is $behind commit(s) behind origin/main (run 'git -C $LOOM_ROOT fetch && git -C $LOOM_ROOT pull' to refresh)"
    fi
  fi

  # Tagged release on a clean snapshot is fine — just announce it.
  if [[ "$branch" == "HEAD" ]] && [[ -n "$tag_match" ]] && [[ -z "$stale" ]]; then
    info "Loom source is at tagged release: $tag_match (detached HEAD)"
    return 0
  fi

  # Clean main, up to date — nothing to say.
  if [[ "$branch" == "main" ]] && [[ -z "$stale" ]]; then
    return 0
  fi

  warning "Loom source checkout is not on a clean main:"
  if [[ "$branch" != "main" ]]; then
    if [[ "$branch" == "HEAD" ]] && [[ -n "$tag_match" ]]; then
      echo "    detached HEAD on tag: $tag_match (no branch)"
    elif [[ "$branch" == "HEAD" ]]; then
      echo "    detached HEAD (no branch, no matching tag)"
    else
      echo "    branch: $branch (expected: main)"
    fi
  fi
  [[ -n "$stale" ]] && echo "    $stale"
  echo "  Source path: $LOOM_ROOT"

  if [[ "$ALLOW_NON_MAIN_SOURCE" == "true" ]]; then
    info "Continuing anyway (--allow-non-main-source)"
    return 0
  fi

  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    error "Refusing to install from non-main / stale source in --yes mode. Pass --allow-non-main-source to override."
  fi

  local reply=""
  read -r -p "Proceed with this source checkout anyway? [y/N] " -n 1 reply
  echo ""
  if [[ ! "$reply" =~ ^[Yy]$ ]]; then
    error "Aborted by user. Switch to main and pull, or pass --allow-non-main-source."
  fi
}

# Cleanup function called on error
cleanup_on_error() {
  local exit_code=$?
  if [[ $exit_code -ne 0 ]]; then
    echo ""
    warning "Installation failed at step: ${CURRENT_STEP:-unknown}"

    if [[ -n "${WORKTREE_PATH:-}" ]] && [[ -d "${TARGET_PATH}/${WORKTREE_PATH}" ]]; then
      info "Cleaning up worktree: ${WORKTREE_PATH}..."
      cd "$TARGET_PATH" 2>/dev/null || true
      git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
      git worktree prune 2>/dev/null || true
      rm -rf "${TARGET_PATH}/${WORKTREE_PATH}"
      if [[ -n "${BRANCH_NAME:-}" ]]; then
        git branch -D "${BRANCH_NAME}" 2>/dev/null || true

        # Delete the remote branch too if create-pr.sh pushed it before
        # failing (e.g. fail at the gh-pr-create step left vibesql/kicad-tools
        # with orphan remote branches in #3244). Restrict to our install branch
        # prefix so unrelated branches are never touched.
        if [[ "${BRANCH_NAME}" =~ ^feature/loom-install-v ]]; then
          if git ls-remote --heads origin "${BRANCH_NAME}" 2>/dev/null | grep -q .; then
            info "Deleting orphan remote branch: ${BRANCH_NAME}"
            git push origin --delete "${BRANCH_NAME}" 2>/dev/null || \
              warning "Could not delete remote branch ${BRANCH_NAME} (manual cleanup may be needed)"
          fi
        fi
      fi
    fi

    echo ""
    error "Installation did not complete. See above for details."
  fi
}

trap cleanup_on_error EXIT
trap 'exit 130' SIGINT
trap 'exit 143' SIGTERM

# Validate arguments
if [[ -z "$TARGET_PATH" ]]; then
  error "Target repository path required\nUsage: $0 [--yes|-y] /path/to/target-repo"
fi

# Auto-detect non-interactive mode when stdin is not a TTY (e.g. `curl | bash`,
# CI pipelines, or any redirected stdin). Without this guard, interactive
# `read -p` prompts fail under `set -euo pipefail`, which trips the EXIT
# cleanup trap and rolls back the entire installation. The explicit `--yes`
# flag is still honored when set on a TTY.
if [[ "$NON_INTERACTIVE" != "true" ]] && [[ ! -t 0 ]]; then
  NON_INTERACTIVE=true
  info "Detected non-interactive stdin (not a TTY) — running in non-interactive mode"
fi

# Export for sub-scripts
export NON_INTERACTIVE

# Source-state guard (#3327): before doing any heavy work, verify that the
# Loom source checkout is on a clean main. Refuse / prompt / continue based
# on flags + interactive vs. --yes mode. Runs after NON_INTERACTIVE finalization
# so the refusal logic sees the effective value (including TTY auto-detect).
check_source_state

# Resolve target to absolute path (git repository root, not worktree)
TARGET_PATH="$(cd "$TARGET_PATH" 2>/dev/null && pwd)" || \
  error "Target path does not exist: $TARGET_PATH"

# Snapshot the pre-install working tree state. The post-install verification
# (near the end of this script) compares against this snapshot so that the
# user's pre-existing dirty state is not incorrectly flagged as "unstaged
# changes after installation". Without this snapshot, the installer would
# recommend destructive cleanup commands (git restore --staged . && git
# checkout -- .) that destroy the user's uncommitted work.
PRE_INSTALL_STATUS=""
if git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  PRE_INSTALL_STATUS=$(git -C "$TARGET_PATH" status --porcelain 2>/dev/null || true)
fi

# Check if target is a git repository - offer to initialize if not
if ! git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    # In non-interactive mode, require --init-git flag for git initialization
    error "Target path is not a git repository: $TARGET_PATH\n       Use interactive mode to initialize git, or run 'git init' first."
  fi

  echo ""
  warning "$TARGET_PATH is not a git repository."
  echo ""
  echo "Would you like to initialize git and set up a remote repository?"
  echo ""
  echo "This will:"
  echo "  1. Run 'git init' in the directory"
  echo "  2. Create a sensible .gitignore file"
  echo "  3. Create an initial commit"
  echo "  4. Create a remote repository and set up the origin (required for Full Install)"
  echo ""
  read -r -p "Initialize git and remote? [y/N] " -n 1 INIT_GIT
  echo ""

  if [[ ! $INIT_GIT =~ ^[Yy]$ ]]; then
    error "Full Install requires a git repository with a remote.\n       Run 'git init' and set up a remote manually, or use Quick Install."
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

# Loom runtime state (added by loom-daemon init)
# These patterns are also managed by loom-daemon's update_gitignore().
GITIGNORE
    success "Created .gitignore"
  fi

  # Create initial commit
  info "Creating initial commit..."
  git add -A
  git commit -m "Initial commit" --quiet || error "Failed to create initial commit"
  success "Initial commit created"
  echo ""

  # Remote repository creation is required for Full Install
  info "Full Install requires a remote repository (GitHub or Gitea)."
  echo ""

  # Check if gh CLI is available (needed for GitHub repo creation)
  if command -v gh &> /dev/null; then
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
      error "Failed to create repository. Cannot proceed with Full Install."
    fi
  else
    warning "GitHub CLI (gh) not available."
    info "For GitHub repos: install gh CLI (brew install gh)"
    info "For Gitea repos: create the repository manually and add the remote:"
    echo "  git remote add origin https://your-gitea-instance/owner/repo.git"
    echo "  git push -u origin main"
    error "Cannot auto-create repository without gh CLI. Set up the remote manually and re-run."
  fi
  echo ""
fi

# If target is inside a worktree, resolve to the main repository root
# git worktree list --porcelain returns the main worktree first
# Note: Use head -4 before grep to avoid SIGPIPE when repo has many worktrees
# (grep -m1 exits early, causing SIGPIPE to git with pipefail enabled)
MAIN_WORKTREE=$(git -C "$TARGET_PATH" worktree list --porcelain 2>/dev/null | head -4 | grep -m1 '^worktree ' | cut -d' ' -f2- || true)
if [[ -n "$MAIN_WORKTREE" ]] && [[ "$TARGET_PATH" != "$MAIN_WORKTREE" ]]; then
  warning "Target path is inside a worktree: $TARGET_PATH"
  info "Resolving to main repository root: $MAIN_WORKTREE"
  TARGET_PATH="$MAIN_WORKTREE"
fi

# ============================================================================
# PRE-FLIGHT: Active Loom session detection (issue #3331)
# ============================================================================
# Refuse to install on top of a live Loom session unless the operator passes
# --allow-active-session. This guards against installing while a daemon is
# running or builders are actively in-flight (state corruption, lost work).
#
# Indicators (any one triggers detection):
#   1. .loom/daemon-loop.pid exists AND PID is alive (kill -0 / ps -p check)
#   2. .loom/daemon-state.json has "running": true AND was updated within the
#      last 5 minutes
#   3. Any .loom/worktrees/issue-N directory with mtime activity in the last
#      5 minutes (in-flight builder)
#
# --force does NOT imply --allow-active-session by design: --force is for
# overwriting files, not for racing with live processes. The operator must
# opt in to both independently.
ACTIVE_SESSION_CHECK="$LOOM_ROOT/scripts/install/check-active-session.sh"
if [[ -x "$ACTIVE_SESSION_CHECK" ]]; then
  if ! "$ACTIVE_SESSION_CHECK" "$TARGET_PATH"; then
    if [[ "$ALLOW_ACTIVE_SESSION" == "true" ]]; then
      warning "Continuing despite active session (--allow-active-session was passed)"
      echo ""
    else
      echo "" >&2
      error "Refusing to install: an active Loom session was detected in the target.

To proceed:
  • Stop the running daemon:   cd '$TARGET_PATH' && ./.loom/scripts/daemon.sh stop
  • Wait for in-flight builders to finish, OR
  • Pass --allow-active-session to override this guard (use with caution).

Note: --force does NOT bypass this check; --allow-active-session must be set explicitly."
    fi
  fi
fi

# Dogfood mode detection (issue #3311):
# When installing Loom *on* the Loom source repo itself, copying
# defaults/.claude/agents/ into .claude/agents/ would create silent drift
# from the source of truth. Instead, replace the copied directory with a
# gitignored symlink so the in-repo .claude/agents/ always reflects the
# committed defaults/.claude/agents/ tree.
#
# Detection: TARGET_PATH equals LOOM_ROOT (the same git repo). Operator can
# force on/off with --dogfood / --no-dogfood. Defaults to auto-detect.
if [[ -z "$DOGFOOD_MODE" ]]; then
  if [[ "$TARGET_PATH" == "$LOOM_ROOT" ]]; then
    DOGFOOD_MODE="true"
    info "Detected dogfood install (target == Loom source repo): .claude/agents will be symlinked"
  else
    DOGFOOD_MODE="false"
  fi
elif [[ "$DOGFOOD_MODE" == "true" ]] && [[ "$TARGET_PATH" != "$LOOM_ROOT" ]]; then
  warning "--dogfood specified but target ($TARGET_PATH) is not the Loom source repo ($LOOM_ROOT)"
  warning "Dogfood symlink would point outside the target repository; disabling dogfood mode"
  DOGFOOD_MODE="false"
fi

# ============================================================================
# PRE-INSTALLATION: Clean existing installation if --clean flag was passed
# ============================================================================
if [[ "$CLEAN_FIRST" == "true" ]]; then
  header "Pre-Installation: Running Uninstall"
  echo ""

  # Check if Loom is installed (has .loom directory)
  if [[ -d "$TARGET_PATH/.loom" ]]; then
    # Preserve uncommitted user changes before uninstall
    # The uninstall --local mode runs 'git add -A' which would stage user changes
    # along with uninstall deletions, and our cleanup would discard them
    STASHED_USER_CHANGES=false
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      warning "Working tree has uncommitted changes"
      info "Stashing user changes to preserve them during clean install..."
      if git -C "$TARGET_PATH" stash push -m "loom-install: preserving user changes before --clean" 2>/dev/null; then
        STASHED_USER_CHANGES=true
        success "User changes stashed"
      else
        warning "Failed to stash changes - uncommitted changes may be lost during --clean install"
        warning "Consider committing your changes first, then retry"
      fi
    fi

    info "Running local uninstall to clean existing installation..."

    # Build uninstall flags from current flags
    # Always use --local so removal happens in working directory (not a worktree)
    # This way the install worktree captures both removals and additions in one PR
    # Always use --clean so unknown files in managed directories are removed
    UNINSTALL_FLAGS="--local --clean"
    if [[ "$NON_INTERACTIVE" == "true" ]]; then
      UNINSTALL_FLAGS="$UNINSTALL_FLAGS --yes"
    fi

    # Run uninstall script in local mode
    "$LOOM_ROOT/scripts/uninstall-loom.sh" $UNINSTALL_FLAGS "$TARGET_PATH" || \
      error "Clean install failed - uninstall step encountered an error"

    echo ""
    success "Uninstall complete - proceeding with fresh installation"

    # Clean up staged deletions from uninstall
    # The uninstall ran in --local mode, staging file deletions directly in the
    # main working directory. The fresh install will happen in a worktree, so
    # main must stay clean to avoid leftover staged changes after completion.
    info "Cleaning staged changes from uninstall..."

    CLEANUP_FAILED=false

    if ! git -C "$TARGET_PATH" restore --staged . 2>/dev/null; then
      warning "Failed to unstage changes from uninstall"
      CLEANUP_FAILED=true
    fi

    if ! git -C "$TARGET_PATH" checkout -- . 2>/dev/null; then
      warning "Failed to restore files from uninstall"
      CLEANUP_FAILED=true
    fi

    # Also clean any untracked files left by the uninstall process
    # Only clean Loom-owned directories — NOT .claude/ which is a shared directory
    # that may contain custom project-specific commands not installed by Loom
    git -C "$TARGET_PATH" clean -fd .loom/ .codex/ .github/labels.yml 2>/dev/null || true

    # Verify the working tree is clean
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      CLEANUP_FAILED=true
    fi

    if [[ "$CLEANUP_FAILED" == "true" ]]; then
      echo ""
      warning "Working tree may not be fully clean after uninstall cleanup"
      warning "Remaining changes:"
      git -C "$TARGET_PATH" status --short 2>/dev/null || true
      echo ""
      warning "To fix manually after installation:"
      warning "  cd $TARGET_PATH"
      warning "  git restore --staged ."
      warning "  git checkout -- ."
    else
      success "Working tree is clean after uninstall cleanup"
    fi

    # Restore stashed user changes
    if [[ "$STASHED_USER_CHANGES" == "true" ]]; then
      info "Restoring stashed user changes..."
      if git -C "$TARGET_PATH" stash pop 2>/dev/null; then
        success "User changes restored"
      else
        warning "Failed to restore stashed user changes automatically"
        warning "Run 'git stash pop' in $TARGET_PATH to recover your changes"
      fi
    fi

    echo ""
  else
    info "No existing Loom installation detected - proceeding with fresh install"
    echo ""
  fi
fi

echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║           Loom Installation - Full Workflow               ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

info "Target: $TARGET_PATH"
echo ""

# Check required dependencies
header "Checking Dependencies"
echo ""

MISSING_DEPS=()

# Check if node is available (needed for version extraction)
if command -v node &> /dev/null; then
  success "node: $(node --version)"
else
  MISSING_DEPS+=("node")
fi

# Check for pnpm (used by the JS workspace; no longer required for the daemon build)
if command -v pnpm &> /dev/null; then
  success "pnpm: $(pnpm --version)"
else
  MISSING_DEPS+=("pnpm")
fi

# Check for cargo (needed to build daemon)
if command -v cargo &> /dev/null; then
  success "cargo: $(cargo --version | head -1)"
else
  MISSING_DEPS+=("cargo")
fi

# Check for Python 3.10+ (needed for loom-tools)
PYTHON_FOUND=false
for py in python3.12 python3.11 python3.10 python3; do
  if command -v "$py" &>/dev/null; then
    version=$("$py" --version 2>&1 | grep -oE '[0-9]+\.[0-9]+' | head -1)
    major=$(echo "$version" | cut -d. -f1)
    minor=$(echo "$version" | cut -d. -f2)
    if [[ "$major" -ge 3 ]] && [[ "$minor" -ge 10 ]]; then
      success "python: $("$py" --version 2>&1)"
      PYTHON_FOUND=true
      break
    fi
  fi
done
if [[ "$PYTHON_FOUND" != "true" ]]; then
  MISSING_DEPS+=("python3.10+")
fi

echo ""

# Report missing dependencies
if [[ ${#MISSING_DEPS[@]} -gt 0 ]]; then
  echo -e "${RED}✗ Missing required dependencies: ${MISSING_DEPS[*]}${NC}"
  echo ""
  info "Please install the missing dependencies:"
  for dep in "${MISSING_DEPS[@]}"; do
    case "$dep" in
      node)
        echo "  • Node.js: brew install node (or https://nodejs.org/)"
        ;;
      pnpm)
        echo "  • pnpm: npm install -g pnpm (or https://pnpm.io/installation)"
        ;;
      cargo)
        echo "  • Rust/Cargo: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        ;;
      python3.10+)
        echo "  • Python 3.10+: brew install python@3.12 (or https://python.org/)"
        ;;
    esac
  done
  echo ""
  error "Cannot proceed without required dependencies"
fi

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
echo ""

# Export environment variables for installation scripts
export LOOM_VERSION
export LOOM_COMMIT
export LOOM_ROOT
export ALLOW_STALE_TARGET

# Export FORCE_AUTO_MERGE for create-pr.sh
# When --force is passed, also enable auto-merge on the installation PR
if [[ "$FORCE_OVERWRITE" == "true" ]]; then
  export FORCE_AUTO_MERGE=true
else
  export FORCE_AUTO_MERGE=false
fi

# Export SKIP_TARGET_CI for create-pr.sh (issue #3333).
# When --skip-target-ci is passed, the install PR title and commit subject
# are prepended with `[skip ci]` — the universal GitHub-native CI skip
# directive. Default-off so install PRs into repos with required-check
# rulesets do not silently break CI gating.
export SKIP_TARGET_CI

# Verify installation scripts directory exists
if [[ ! -d "$LOOM_ROOT/scripts/install" ]]; then
  error "Installation scripts directory not found: $LOOM_ROOT/scripts/install"
fi

# ============================================================================
# PRE-FLIGHT: Idempotency Checks
# ============================================================================
# Skip idempotency checks if --force or --clean flags are set
if [[ "$FORCE_OVERWRITE" != "true" ]] && [[ "$CLEAN_FIRST" != "true" ]]; then
  header "Pre-flight: Idempotency Checks"
  echo ""

  # Check 1: Is Loom already installed with the same version?
  # Layered version detection — prefer .loom/install-metadata.json (deterministic
  # JSON written at install time), fall back to .loom/CLAUDE.md (substituted
  # template), then root CLAUDE.md (legacy). Reject `{{...}}` placeholder leaks
  # from stale/corrupted installs.
  INSTALLED_VERSION=""

  # Source 1: .loom/install-metadata.json (preferred — deterministic JSON)
  if [[ -f "$TARGET_PATH/.loom/install-metadata.json" ]]; then
    if command -v jq >/dev/null 2>&1; then
      INSTALLED_VERSION=$(jq -r '.loom_version // empty' "$TARGET_PATH/.loom/install-metadata.json" 2>/dev/null || true)
    else
      # Robust grep fallback when jq is unavailable
      INSTALLED_VERSION=$(grep -o '"loom_version"[[:space:]]*:[[:space:]]*"[^"]*"' "$TARGET_PATH/.loom/install-metadata.json" 2>/dev/null | head -1 | sed 's/.*"loom_version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' || true)
    fi
  fi

  # Source 2: .loom/CLAUDE.md (substituted template)
  if [[ -z "$INSTALLED_VERSION" ]] && [[ -f "$TARGET_PATH/.loom/CLAUDE.md" ]]; then
    INSTALLED_VERSION=$(grep 'Loom Version' "$TARGET_PATH/.loom/CLAUDE.md" 2>/dev/null | head -1 | sed 's/.*Loom Version.*: //' | sed 's/\*//g' | tr -d '[:space:]' || true)
  fi

  # Source 3: root CLAUDE.md (legacy fallback)
  if [[ -z "$INSTALLED_VERSION" ]] && [[ -f "$TARGET_PATH/CLAUDE.md" ]]; then
    INSTALLED_VERSION=$(grep 'Loom Version' "$TARGET_PATH/CLAUDE.md" 2>/dev/null | head -1 | sed 's/.*Loom Version.*: //' | sed 's/\*//g' | tr -d '[:space:]' || true)
  fi

  # Reject placeholder leaks (e.g. literal `{{LOOM_VERSION}}` from corrupted/stale
  # template that was never substituted).
  if [[ "$INSTALLED_VERSION" =~ ^\{\{.*\}\}$ ]]; then
    INSTALLED_VERSION="unknown (stale template — re-run with --force to fix)"
  fi

  if [[ -n "$INSTALLED_VERSION" ]]; then
    if [[ "$INSTALLED_VERSION" == "$LOOM_VERSION" ]]; then
      info "Loom v${LOOM_VERSION} is already installed in this repository."
      info "Use --force to reinstall or --clean for a fresh install."
      echo ""

      # Disable error trap and exit successfully
      trap - EXIT SIGINT SIGTERM
      exit 0
    else
      info "Existing Loom installation detected: v${INSTALLED_VERSION}"
      info "Upgrading to: v${LOOM_VERSION}"
    fi
  fi

  # Check 2: Is there already an open installation PR?
  # Extract owner/repo from URL (handles GitHub, Gitea, and other forges)
  _ORIGIN_URL=$(git -C "$TARGET_PATH" config --get remote.origin.url 2>/dev/null || true)
  REPO_NWOPATH=$(echo "$_ORIGIN_URL" | sed -E 's/\.git$//; s#^.*[:/]([^/]+/[^/]+)$#\1#' || true)
  if [[ -n "$REPO_NWOPATH" ]] && [[ "$REPO_NWOPATH" =~ ^[^/]+/[^/]+$ ]]; then
    EXISTING_INSTALL_PR=$(gh pr list -R "$REPO_NWOPATH" --state open --search "Install Loom" --json url,headRefName --jq '
      [.[] | select(.headRefName | startswith("feature/loom-install-"))][0].url' 2>/dev/null || true)

    if [[ -n "$EXISTING_INSTALL_PR" ]]; then
      warning "An open Loom installation PR already exists:"
      info "  ${EXISTING_INSTALL_PR}"
      echo ""

      if [[ "$NON_INTERACTIVE" == "true" ]]; then
        info "Non-interactive mode: Skipping duplicate installation."
        info "Use --force to create a new PR regardless."
        echo ""
        trap - EXIT SIGINT SIGTERM
        exit 0
      else
        read -r -p "Create a new installation PR anyway? [y/N] " -n 1 CREATE_ANYWAY
        echo ""
        if [[ ! $CREATE_ANYWAY =~ ^[Yy]$ ]]; then
          info "Aborting. Merge or close the existing PR first, or use --force."
          trap - EXIT SIGINT SIGTERM
          exit 0
        fi
      fi
    fi
  fi

  success "Idempotency checks passed"
  echo ""
fi

# ============================================================================
# STEP 1: Validate Target Repository
# ============================================================================
CURRENT_STEP="Validate Target"
header "Step 1: Validating Target Repository"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/validate-target.sh" ]]; then
  error "Installation script not found: validate-target.sh"
fi

"$LOOM_ROOT/scripts/install/validate-target.sh" "$TARGET_PATH" || \
  error "Target validation failed"

echo ""

# ============================================================================
# STEP 2: Create Installation Worktree
# ============================================================================
CURRENT_STEP="Create Worktree"
header "Step 2: Creating Installation Worktree"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-worktree.sh" ]]; then
  error "Installation script not found: create-worktree.sh"
fi

WORKTREE_OUTPUT=$("$LOOM_ROOT/scripts/install/create-worktree.sh" "$TARGET_PATH") || \
  error "Failed to create worktree"

# Parse output: WORKTREE_PATH|BRANCH_NAME|BASE_BRANCH
WORKTREE_PATH=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f1)
BRANCH_NAME=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f2)
BASE_BRANCH=$(echo "$WORKTREE_OUTPUT" | cut -d'|' -f3)

# Validate worktree path format (should be relative path starting with .loom/worktrees/)
if [[ ! "$WORKTREE_PATH" =~ ^\.loom/worktrees/ ]]; then
  error "Invalid worktree path returned: $WORKTREE_PATH"
fi

if [[ ! -d "$TARGET_PATH/$WORKTREE_PATH" ]]; then
  error "Worktree was not created: $TARGET_PATH/$WORKTREE_PATH"
fi

info "Worktree: $WORKTREE_PATH"
info "Branch: $BRANCH_NAME"
info "Base branch: $BASE_BRANCH"
echo ""

# ============================================================================
# STEP 3: Initialize Loom Configuration
# ============================================================================
CURRENT_STEP="Initialize Loom"
header "Step 3: Initializing Loom Configuration"
echo ""

# Build loom-daemon (always rebuild to ensure binary matches current source)
# Cargo's incremental compilation makes this fast (~1-2s) when nothing changed
info "Building loom-daemon..."
cd "$LOOM_ROOT"
# Inlined from package.json `daemon:build` script to decouple from pnpm preflight (issue #3252).
# The daemon is pure Rust; invoking via pnpm forced lockfile/workspace/build-script validation
# of the loom source checkout, causing unrelated JS-toolchain drift to abort installation.
cargo build --package loom-daemon --release || error "Failed to build loom-daemon"
# Copy to architecture-specific name (matches release artifact naming)
rm -f target/release/loom-daemon-aarch64-apple-darwin
cp target/release/loom-daemon target/release/loom-daemon-aarch64-apple-darwin

success "loom-daemon binary ready"

# Log the daemon binary identity so a stale binary can be diagnosed
# post-hoc when an install regression is reported (issue #3287).
DAEMON_VERSION=$("$LOOM_ROOT/target/release/loom-daemon" --version 2>/dev/null || echo "(unknown)")
info "loom-daemon binary: $DAEMON_VERSION"

# Run loom-daemon init in the worktree
cd "$TARGET_PATH/$WORKTREE_PATH"

# Use --force to overwrite existing installation when requested (--force or --clean flags)
# Otherwise, use merge mode to preserve custom project roles/commands
# Use --defaults to point to Loom's defaults directory
INIT_FLAGS=""
if [ "$FORCE_OVERWRITE" = true ] || [ "$CLEAN_FIRST" = true ]; then
  INIT_FLAGS="--force"
fi
"$LOOM_ROOT/target/release/loom-daemon" init $INIT_FLAGS --defaults "$LOOM_ROOT/defaults" . || \
  error "loom-daemon init failed"

echo ""

# Dogfood mode (issue #3311): replace the copied .claude/agents/ tree in the
# main checkout with a symlink to defaults/.claude/agents/. This avoids silent
# drift between defaults/ and .claude/agents/ when working *on* the Loom
# source repo. The .gitignore entry for `.claude/agents` (committed) ensures
# git status stays clean and the install PR does not stage agent files.
#
# Why operate on $TARGET_PATH (main checkout) rather than the install worktree:
#   - The relative symlink target `../defaults/.claude/agents` only resolves
#     correctly when `.claude/` sits next to `defaults/` (i.e. at the repo
#     root, not inside `.loom/worktrees/loom-install-*/`).
#   - The install worktree's `.claude/agents/` files are now gitignored, so
#     they will not be staged by `git add -A` in create-pr.sh — leaving them
#     in place is harmless and avoids a destructive delete inside the
#     worktree before the PR is created.
if [[ "$DOGFOOD_MODE" == "true" ]]; then
  info "Dogfood mode: ensuring .claude/agents symlink in $TARGET_PATH..."
  DOGFOOD_LINK_PATH="$TARGET_PATH/.claude/agents"
  DOGFOOD_LINK_TARGET="../defaults/.claude/agents"
  DOGFOOD_ABS_TARGET="$TARGET_PATH/defaults/.claude/agents"

  if [[ ! -d "$DOGFOOD_ABS_TARGET" ]]; then
    warning "Dogfood symlink target does not exist: $DOGFOOD_ABS_TARGET"
    warning "Skipping symlink creation; .claude/agents may be missing or stale"
  else
    mkdir -p "$TARGET_PATH/.claude"
    if [[ -L "$DOGFOOD_LINK_PATH" ]]; then
      EXISTING_TARGET=$(readlink "$DOGFOOD_LINK_PATH")
      if [[ "$EXISTING_TARGET" == "$DOGFOOD_LINK_TARGET" ]]; then
        success ".claude/agents symlink already correct (-> $DOGFOOD_LINK_TARGET)"
      else
        info "Updating .claude/agents symlink: $EXISTING_TARGET -> $DOGFOOD_LINK_TARGET"
        rm -f "$DOGFOOD_LINK_PATH"
        ln -s "$DOGFOOD_LINK_TARGET" "$DOGFOOD_LINK_PATH"
        success "Updated .claude/agents symlink"
      fi
    elif [[ -e "$DOGFOOD_LINK_PATH" ]]; then
      # Real directory or file occupies the path — replace with symlink.
      # Safe because (a) defaults/.claude/agents/ has the canonical content,
      # and (b) .claude/agents is gitignored so any local edits would not have
      # been committed anyway. Preserve any local-only files by refusing to
      # delete when the directory contains files not present in defaults.
      LOCAL_ONLY_FILES=$(comm -23 \
        <(cd "$DOGFOOD_LINK_PATH" 2>/dev/null && find . -type f | sort) \
        <(cd "$DOGFOOD_ABS_TARGET" 2>/dev/null && find . -type f | sort) \
        2>/dev/null || true)
      if [[ -n "$LOCAL_ONLY_FILES" ]]; then
        warning ".claude/agents contains local-only files not present in defaults:"
        echo "$LOCAL_ONLY_FILES" | sed 's/^/    /'
        warning "Refusing to replace with symlink. Move or commit these files, then re-run."
      else
        info "Replacing copied .claude/agents/ directory with symlink to defaults/..."
        rm -rf "$DOGFOOD_LINK_PATH"
        ln -s "$DOGFOOD_LINK_TARGET" "$DOGFOOD_LINK_PATH"
        success "Replaced .claude/agents/ with symlink -> $DOGFOOD_LINK_TARGET"
      fi
    else
      ln -s "$DOGFOOD_LINK_TARGET" "$DOGFOOD_LINK_PATH"
      success "Created .claude/agents symlink -> $DOGFOOD_LINK_TARGET"
    fi
  fi
  echo ""

  # Dogfood mode (issue #3565): materialize loom's live `.claude/commands/loom/`
  # as a real COPY of `defaults/.claude/commands/loom/`, NOT a symlink into
  # `defaults/`.
  #
  # Why a copy and not a symlink (unlike .claude/agents above):
  #   A symlink `.claude/commands -> ../defaults/.claude/commands` (or a
  #   per-file symlink into defaults/) still redirects on-disk writes into the
  #   shipped `defaults/` tree. A co-installed tool that writes
  #   `.claude/commands/<ns>/foo.md` would therefore pollute loom's own
  #   distribution artifact (this is exactly how it bit us — see #3565).
  #   Materializing a REAL gitignored directory makes loom's live
  #   `.claude/commands/` a pure destination like every consumer's: sibling
  #   namespaces land harmlessly next to `loom/` and never touch `defaults/`.
  #
  # The command markdown carries no init-substituted placeholders
  # ({{LOOM_VERSION}} etc.), so a plain copy is byte-identical to a consumer
  # install — no template substitution needed for this layer.
  #
  # `.claude/commands` is gitignored in loom's own committed .gitignore (added
  # in #3565), so this real directory is never staged by `git add -A`. The
  # daemon's consumer `update_gitignore` list is intentionally NOT touched —
  # consumer repos keep `.claude/commands/loom/` tracked.
  info "Dogfood mode: materializing .claude/commands/loom copy in $TARGET_PATH..."
  CMD_SRC="$TARGET_PATH/defaults/.claude/commands/loom"
  CMD_LIVE_DIR="$TARGET_PATH/.claude/commands"
  CMD_LIVE_LOOM="$CMD_LIVE_DIR/loom"

  if [[ ! -d "$CMD_SRC" ]]; then
    warning "Dogfood commands source does not exist: $CMD_SRC"
    warning "Skipping .claude/commands materialization; commands may be missing or stale"
  else
    mkdir -p "$TARGET_PATH/.claude"

    # If `.claude/commands` is still the legacy whole-dir symlink into
    # defaults/, remove it so we can build a real destination directory in its
    # place. Any sibling namespaces from co-installed tools already live in a
    # real dir and are preserved.
    if [[ -L "$CMD_LIVE_DIR" ]]; then
      info "Removing legacy .claude/commands symlink -> $(readlink "$CMD_LIVE_DIR")"
      rm -f "$CMD_LIVE_DIR"
    fi
    mkdir -p "$CMD_LIVE_DIR"

    # Build the fresh copy in a temp dir on the same filesystem, then swap it
    # into place with an atomic rename so an in-progress Claude Code session
    # never observes a missing or half-written `.claude/commands/loom/`. Only
    # the `loom/` subdir is swapped — sibling namespaces under
    # `.claude/commands/` are untouched, and `.claude/commands/` itself is
    # always present.
    CMD_TMP="$(mktemp -d "$TARGET_PATH/.claude/.commands-bootstrap.XXXXXX")"
    cp -R "$CMD_SRC/." "$CMD_TMP/loom-new"
    if [[ -e "$CMD_LIVE_LOOM" || -L "$CMD_LIVE_LOOM" ]]; then
      # Move the old copy aside first (brief; new content is byte-identical).
      rm -rf "$CMD_TMP/loom-old"
      mv "$CMD_LIVE_LOOM" "$CMD_TMP/loom-old"
    fi
    mv "$CMD_TMP/loom-new" "$CMD_LIVE_LOOM"
    rm -rf "$CMD_TMP"
    success "Materialized .claude/commands/loom/ (real copy of defaults/, gitignored)"
  fi
  echo ""
fi

# Configure git hooks path so .githooks/ pre-commit works without husky/npx
info "Configuring git hooks path..."
git config core.hooksPath .githooks
success "Set core.hooksPath to .githooks"
echo ""

# Copy hooks to target (settings.json references .loom/hooks/guard-destructive.sh)
info "Installing hooks..."
if [[ -d "$LOOM_ROOT/defaults/hooks" ]]; then
  mkdir -p .loom/hooks
  for hook_file in "$LOOM_ROOT/defaults/hooks/"*.sh; do
    [[ -f "$hook_file" ]] || continue
    hook_name=$(basename "$hook_file")
    if [[ -f ".loom/hooks/$hook_name" ]] && [[ "$FORCE_OVERWRITE" != "true" ]] && [[ "$CLEAN_FIRST" != "true" ]]; then
      info "Skipping existing hook: $hook_name (use --force to overwrite)"
    else
      cp "$hook_file" ".loom/hooks/$hook_name"
      chmod +x ".loom/hooks/$hook_name"
      success "Installed hook: $hook_name"
    fi
  done
else
  warning "No hooks directory found in defaults"
fi
echo ""

# Copy default config files (skill-routes.json template, etc.)
info "Installing default config files..."
if [[ -d "$LOOM_ROOT/defaults/config" ]]; then
  mkdir -p .loom/config
  for config_file in "$LOOM_ROOT/defaults/config/"*.json; do
    [[ -f "$config_file" ]] || continue
    config_name=$(basename "$config_file")
    if [[ -f ".loom/config/$config_name" ]] && [[ "$FORCE_OVERWRITE" != "true" ]] && [[ "$CLEAN_FIRST" != "true" ]]; then
      info "Skipping existing config: $config_name (use --force to overwrite)"
    else
      cp "$config_file" ".loom/config/$config_name"
      success "Installed config: $config_name"
    fi
  done
else
  info "No config directory found in defaults (skipping)"
fi
echo ""

# Set up Python tools (loom-tools package)
# This creates a virtual environment in loom-tools/.venv and installs loom-status, loom-clean, etc.
info "Setting up Python tools..."
if [[ -x "$LOOM_ROOT/scripts/install/setup-python-tools.sh" ]]; then
  if "$LOOM_ROOT/scripts/install/setup-python-tools.sh" --loom-root "$LOOM_ROOT"; then
    success "Python tools installed"
  else
    warning "Python tools setup failed (non-fatal for installation)"
    info "Run manually: $LOOM_ROOT/scripts/install/setup-python-tools.sh --loom-root $LOOM_ROOT"
    info "Without Python tools, loom-status and some scripts will not work."
  fi
else
  warning "Python setup script not found"
  info "Python tools (loom-status, loom-clean, etc.) may not be available."
fi

# Store Loom source repository path for wrapper scripts
# This enables scripts in the target repo to find loom-tools in the source repo
info "Recording Loom source path..."
echo "$LOOM_ROOT" > .loom/loom-source-path
# Also write to target repo root — the worktree copy is gitignored and will be
# lost when the installation worktree is cleaned up after PR merge
echo "$LOOM_ROOT" > "$TARGET_PATH/.loom/loom-source-path"
success "Loom source path recorded"

# Store installation metadata (commit hash moved here from CLAUDE.md for idempotency)
# Also record the list of installed files so the uninstaller knows exactly what to remove.
# This prevents the uninstaller from using heuristics and accidentally touching project files.
info "Recording installation metadata..."

# Collect all installed files (relative paths from repo root).
#
# Issue #3450: the old implementation walked the *target* repo with
#   find .loom .claude .codex .github .githooks CLAUDE.md .gitignore
# which over-captured consumer-authored files. The uninstaller trusted that
# manifest as authoritative for deletion and silently destroyed consumer
# .github/workflows/*.yml, .gitignore, and CLAUDE.md. Now we walk
# $LOOM_ROOT/defaults/ (the actual Loom-shipped files) via the
# _emit_installed_files_manifest helper above.
INSTALLED_FILES_JSON="$(_emit_installed_files_manifest)"

# --- Stale-file sweep (upgrade path) ---
# Compare the previous install's file list (from install-metadata.json) against
# the new set. Files present in the old set but not the new set are orphans from
# upstream renames/deletions and must be removed so they don't accumulate.
# Only files the installer originally wrote are candidates — operator-added files
# are never in PREV_INSTALLED_FILES, so they are safe by construction.
#
# Issue #3450 defense-in-depth: v0.7.2 (and earlier) wrote an over-broad
# manifest that included consumer-authored .github/workflows/*.yml, custom
# .gitignore entries, and pre-existing CLAUDE.md. The narrowed manifest no
# longer lists those paths — but the sweep below would happily git-rm them
# as "stale". The carve-out here mirrors the uninstall-side skip list so an
# upgrade from a v0.7.2 install does NOT delete consumer-owned files. See
# scripts/uninstall-loom.sh near the "v0.7.2's over-broad manifest" comment.
#
# Issue #3492 ownership-boundary intersection: in addition to the
# `.github/*`-style allowlist below, every candidate is intersected
# against the CURRENT Loom ownership set (_emit_loom_ownership_set —
# the same path set _emit_installed_files_manifest produces). If a
# stale-manifest entry is NOT in the current ownership set, the current
# defaults/ does not ship it; it is preserved with a warning rather
# than git-rm'd. This defends against pre-#3450 manifests that listed
# consumer-authored .claude/skills/**, .claude/commands/<non-loom>/**,
# etc.
LOOM_OWNERSHIP_SET="$(_emit_loom_ownership_set)"
STALE_FILES=()
PRESERVED_NOT_OWNED=()
if [[ -f "$TARGET_PATH/.loom/install-metadata.json" ]] && command -v jq >/dev/null 2>&1; then
  while IFS= read -r prev_file; do
    [[ -n "$prev_file" ]] || continue

    # Defense-in-depth (#3450, #3480): never sweep files in Loom's
    # "consumer-owned" carve-out, even if they appear in the previous
    # manifest. Mirrors the uninstall-side skip list.
    #
    # .github/ is an ALLOWLIST: only the files Loom actually ships into
    # targets (source of truth: defaults/.github/ as walked by
    # scripts/install/manifest.sh) fall through to the sweep. Everything
    # else under .github/ — consumer workflows, composite actions,
    # dependabot.yml, etc. — is consumer-owned by default and never swept,
    # even when a legacy over-broad manifest (v0.7.x, #3450) lists it.
    # If Loom ever ships new .github/ files (e.g. workflows), add those
    # exact paths here AND in scripts/uninstall-loom.sh.
    case "$prev_file" in
      CLAUDE.md|.gitignore|.claude/settings.json)
        continue
        ;;
      .github/labels.yml|.github/CONFIGURATION.md|.github/ISSUE_TEMPLATE/config.yml|.github/ISSUE_TEMPLATE/task.yml)
        # Loom-shipped — fall through to the sweep.
        ;;
      .github/*)
        # Consumer-owned by default.
        continue
        ;;
    esac

    # Issue #3492: intersect against the current ownership boundary. A
    # path the previous manifest claimed Loom owned but that the current
    # defaults/ no longer ships is consumer-authored (captured by an
    # over-broad pre-#3450 manifest) and must NOT be git-rm'd.
    if [[ -n "$LOOM_OWNERSHIP_SET" ]] \
        && ! printf '%s\n' "$LOOM_OWNERSHIP_SET" | grep -Fxq -- "$prev_file"; then
      PRESERVED_NOT_OWNED+=("$prev_file")
      continue
    fi

    # Is this file still in the new installed set?
    if ! echo "$INSTALLED_FILES_JSON" | grep -qF "\"${prev_file}\""; then
      STALE_FILES+=("$prev_file")
    fi
  done < <(jq -r '.installed_files[]' "$TARGET_PATH/.loom/install-metadata.json")
fi

# Issue #3492: surface preserved paths so operators can audit their tree
# for other pre-#3450 contamination. Single warning per file, no silent
# skip.
if (( ${#PRESERVED_NOT_OWNED[@]} > 0 )); then
  for f in "${PRESERVED_NOT_OWNED[@]}"; do
    warning "preserving ${f} (not owned by current Loom defaults/; likely consumer-authored, captured by pre-#3450 manifest)"
  done
fi

if (( ${#STALE_FILES[@]} > 0 )); then
  echo ""
  info "Stale files from previous Loom install (removed or renamed upstream):"
  for f in "${STALE_FILES[@]}"; do
    echo "    - $f"
  done
  echo ""
  PROCEED_DELETE=true
  if [[ "$NON_INTERACTIVE" != "true" ]] && [[ "$FORCE_OVERWRITE" != "true" ]]; then
    read -r -p "Remove these ${#STALE_FILES[@]} stale file(s)? [y/N] " CONFIRM_DELETE
    [[ "$CONFIRM_DELETE" =~ ^[Yy]$ ]] || PROCEED_DELETE=false
  fi
  if [[ "$PROCEED_DELETE" == "true" ]]; then
    DELETED_COUNT=0
    for f in "${STALE_FILES[@]}"; do
      if git rm --quiet --force "$f" 2>/dev/null; then
        (( DELETED_COUNT++ )) || true
      else
        warning "Could not remove stale file: $f (may have been removed already)"
      fi
    done
    success "Removed $DELETED_COUNT stale file(s) from previous Loom version"
  else
    info "Skipping stale file removal (user declined)"
  fi
fi
# --- End stale-file sweep ---

cat > .loom/install-metadata.json <<METADATA
{
  "loom_version": "${LOOM_VERSION}",
  "loom_commit": "${LOOM_COMMIT}",
  "install_date": "$(date +%Y-%m-%d)",
  "loom_source": "${LOOM_ROOT}",
  "installed_files": ${INSTALLED_FILES_JSON}
}
METADATA
success "Installation metadata recorded ($(echo "$INSTALLED_FILES_JSON" | grep -o '"' | wc -l | awk '{print $1/2}') files tracked)"
echo ""

# Reconcile install-metadata.json against on-disk state.
# Issue #3287: a metadata-vs-disk divergence is itself a hard error — regardless
# of which step caused it (a stale daemon binary, a hostile gitignore, a copy
# race, etc.). This is the cheapest single check that catches every variant.
info "Verifying all metadata-listed files exist on disk..."
MISSING_FROM_METADATA=()
if command -v jq >/dev/null 2>&1; then
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    if [[ ! -e "$path" ]]; then
      MISSING_FROM_METADATA+=("$path")
    fi
  done < <(jq -r '.installed_files[]' .loom/install-metadata.json)
else
  warning "jq not available — skipping metadata reconciliation check"
fi
if (( ${#MISSING_FROM_METADATA[@]} > 0 )); then
  echo "" >&2
  for f in "${MISSING_FROM_METADATA[@]}"; do
    echo "  MISSING: $f" >&2
  done
  error "${#MISSING_FROM_METADATA[@]} file(s) in install-metadata.json are missing from disk — install incomplete (see #3287)"
fi
success "Metadata reconciliation passed (all listed files present)"
echo ""

# Verify no installed lib files are gitignored.
# This catches the hostile-gitignore variant of issue #3287 *after* the
# daemon's own check, providing belt-and-suspenders coverage for users on
# stale daemon binaries that predate the in-daemon gitignore audit.
#
# Issue #3326: use `git check-ignore -v` so we can surface the specific
# `<gitignore-file>:<line>:<pattern>` triple and offer an actionable fix
# instead of a generic "commonly '.loom/' or '.loom/scripts/'" hint.
if [[ -d ".loom/scripts/lib" ]]; then
  info "Checking that installed lib/*.sh files are not gitignored..."
  # -v emits "<gitignore>:<line>:<pattern>\t<file>" for each match.
  IGNORED_LIB_FILES=$(git check-ignore -v .loom/scripts/lib/*.sh 2>/dev/null || true)
  if [[ -n "$IGNORED_LIB_FILES" ]]; then
    # Parse first match to extract gitignore path, line, and pattern for the
    # suggested-fix block. Subsequent matches are still listed verbatim below.
    first_match=$(echo "$IGNORED_LIB_FILES" | head -1)
    pattern_info="${first_match%%$'\t'*}"          # "<gitignore>:<line>:<pattern>"
    pattern="${pattern_info##*:}"                  # "<pattern>"
    gi_path_and_line="${pattern_info%:*}"          # "<gitignore>:<line>"
    line_no="${gi_path_and_line##*:}"              # "<line>"
    gi_file="${gi_path_and_line%:*}"               # "<gitignore>"

    echo "" >&2
    echo "  The following lib files are matched by a .gitignore rule:" >&2
    while IFS=$'\t' read -r match file; do
      [[ -z "$match" && -z "$file" ]] && continue
      # match is "<gitignore>:<line>:<pattern>"
      m_pattern="${match##*:}"
      m_path_and_line="${match%:*}"
      m_line="${m_path_and_line##*:}"
      m_gi="${m_path_and_line%:*}"
      echo "    $file" >&2
      echo "      matched by ${m_gi}:${m_line} pattern '${m_pattern}'" >&2
    done <<<"$IGNORED_LIB_FILES"
    echo "" >&2

    # Suggest a fix based on the shape of the offending pattern.
    # 1. Unanchored single-segment dir (e.g. `lib/`, `bin/`, `share/`): suggest
    #    anchoring to repo root by prefixing with `/`. This is the most common
    #    cause (Python venv-template .gitignores ship with unanchored `lib/`).
    # 2. Loom-wildcard pattern (e.g. `.loom`, `.loom/`, `.loom*`): the user is
    #    explicitly hiding Loom — they need to delete the rule, not anchor it,
    #    because Loom's working dir must be committed.
    # 3. Anything else: generic guidance to narrow or remove the pattern.
    if [[ "$pattern" =~ ^[a-zA-Z0-9_-]+/$ ]]; then
      echo "  Suggested fix: anchor the pattern to the repo root." >&2
      echo "    Change line ${line_no} of ${gi_file} from:  ${pattern}" >&2
      echo "    To:                                          /${pattern}" >&2
    elif [[ "$pattern" == .loom* ]]; then
      echo "  Suggested fix: remove this pattern from ${gi_file}:${line_no}." >&2
      echo "  Loom's working directory (.loom/) must be committed to git." >&2
    else
      echo "  Suggested fix: remove or narrow the pattern '${pattern}' at ${gi_file}:${line_no}." >&2
    fi
    echo "" >&2
    echo "  Then commit the .gitignore fix and re-run the installer." >&2
    error "Installed lib files are gitignored — install would produce a broken commit (see #3287)"
  fi
  success "lib/*.sh files are not gitignored"
  echo ""
fi

# Verify expected files were created
# NOTE: lib/ entries are listed explicitly as a defensive belt-and-suspenders check.
# The recursive verification block below catches any other missing files under
# defaults/scripts/, but listing lib/ here ensures these specific helpers
# (sourced by ~17 other scripts) cannot regress silently. See issue #3220.
EXPECTED_FILES=(
  ".loom/config.json"
  ".loom/roles"
  ".loom/scripts/worktree.sh"
  ".loom/scripts/lib/loom-tools.sh"
  ".loom/scripts/lib/forge-helpers.sh"
  ".loom/scripts/lib/pipe-pane-cmd.sh"
  ".loom/hooks/guard-destructive.sh"
  ".loom/hooks/skill-router.sh"
  "CLAUDE.md"
  ".github/labels.yml"
  ".claude/commands/loom"
  ".claude/agents"
  ".claude/settings.json"
)

info "Verifying installation files..."
for file in "${EXPECTED_FILES[@]}"; do
  if [[ ! -e "$file" ]]; then
    error "Expected file not created: $file"
  fi
done

success "All Loom files installed"
echo ""

# Verify installed scripts match source defaults (recursive walk).
# Missing files are now a HARD ERROR — the previous warning-only behavior
# allowed regressions like #3220 (lib/forge-helpers.sh missing) to slip through.
# Content mismatches remain warnings since template substitution and platform
# differences (line endings, etc.) can cause false positives.
info "Verifying scripts match source..."
VERIFY_MISSING=0
VERIFY_MISMATCHES=0
MISSING_SCRIPTS=()
if [[ -d "$LOOM_ROOT/defaults/scripts" ]] && [[ -d ".loom/scripts" ]]; then
  while IFS= read -r -d '' src_file; do
    rel_path="${src_file#$LOOM_ROOT/defaults/scripts/}"
    dst_file=".loom/scripts/$rel_path"
    if [[ -f "$dst_file" ]]; then
      if ! cmp -s "$src_file" "$dst_file"; then
        warning "Script mismatch: .loom/scripts/$rel_path differs from source"
        VERIFY_MISMATCHES=$((VERIFY_MISMATCHES + 1))
      fi
    else
      MISSING_SCRIPTS+=(".loom/scripts/$rel_path")
      VERIFY_MISSING=$((VERIFY_MISSING + 1))
    fi
  done < <(find "$LOOM_ROOT/defaults/scripts" -type f -print0)
fi
if [[ $VERIFY_MISSING -gt 0 ]]; then
  echo "" >&2
  for missing in "${MISSING_SCRIPTS[@]}"; do
    echo "  MISSING: $missing" >&2
  done
  error "$VERIFY_MISSING script(s) missing from installation — install incomplete (see #3220)"
fi
if [[ $VERIFY_MISMATCHES -gt 0 ]]; then
  warning "$VERIFY_MISMATCHES script(s) had content mismatches — see above"
else
  success "All scripts verified (match source defaults)"
fi
echo ""

# Install Loom CLI wrapper (.loom/bin/loom)
if [[ -f "$LOOM_ROOT/defaults/.loom/bin/loom" ]]; then
  info "Installing Loom CLI wrapper..."
  mkdir -p ".loom/bin"
  cp "$LOOM_ROOT/defaults/.loom/bin/loom" ".loom/bin/loom" || \
    error "Failed to copy Loom CLI"
  chmod +x ".loom/bin/loom"
  success "Installed .loom/bin/loom CLI"
fi
echo ""

# Generate installation manifest (checksum of all installed files)
if [[ -x ".loom/scripts/verify-install.sh" ]]; then
  info "Generating installation manifest..."
  ./.loom/scripts/verify-install.sh generate --quiet || \
    warning "Manifest generation failed (non-fatal)"
  success "Installation manifest generated (.loom/manifest.json)"
fi
echo ""

# ============================================================================
# STEP 4: Sync GitHub Labels
# ============================================================================
CURRENT_STEP="Sync Labels"
header "Step 4: Syncing Workflow Labels"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/sync-labels.sh" ]]; then
  error "Installation script not found: sync-labels.sh"
fi

"$LOOM_ROOT/scripts/install/sync-labels.sh" "$TARGET_PATH/$WORKTREE_PATH" || \
  warning "Label sync had issues but continuing..."

echo ""

# ============================================================================
# STEP 4b: Build MCP Server and Generate Configuration
# ============================================================================
CURRENT_STEP="MCP Setup"
header "Step 4b: MCP Server Setup"
echo ""

info "Building unified MCP server..."
if "$LOOM_ROOT/scripts/setup-mcp.sh"; then
  success "MCP server configured"
else
  warning "MCP setup failed - MCP tools will not be available"
  info "Run manually later: $LOOM_ROOT/scripts/setup-mcp.sh"
fi

echo ""

# ============================================================================
# STEP 5: Configure Branch Rulesets
# ============================================================================
# Optional post-install hardening — wrap the entire block in a non-fatal
# subshell. If anything inside fails (interactive read on non-TTY, missing
# permissions, etc.), the installation continues to PR creation rather than
# triggering the EXIT cleanup trap and rolling back completed work.
CURRENT_STEP="Configure Branch Rulesets"
header "Step 5: Configure Branch Rulesets"
echo ""

step5_branch_rulesets() {
  # Detect default branch
  cd "$TARGET_PATH"
  local DEFAULT_BRANCH
  DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "main")
  info "Detected default branch: ${DEFAULT_BRANCH}"

  # Prompt user
  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    info "Non-interactive mode: Skipping branch ruleset setup"
    info "To configure manually, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
  else
    echo ""
    local REPLY
    read -p "Configure branch ruleset for '${DEFAULT_BRANCH}' branch? (y/N) " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Yy]$ ]]; then
      info "Applying branch ruleset..."

      # Apply ruleset (interactive — setup-branch-protection.sh will prompt on
      # cross-name overlap; LOOM_NON_INTERACTIVE is explicitly unset/false here
      # so the helper presents the Skip/Replace/Update options when overlap
      # is detected — see issue #3216).
      if LOOM_NON_INTERACTIVE=false "$LOOM_ROOT/scripts/install/setup-branch-protection.sh" "$TARGET_PATH" "$DEFAULT_BRANCH"; then
        echo ""
      else
        echo ""
        warning "Failed to configure branch ruleset (may require admin permissions)"
        info "You can configure manually via your forge's Settings > Branch Protection"
      fi
    else
      info "Skipping branch ruleset setup"
      info "To configure later, run: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH $DEFAULT_BRANCH"
    fi
  fi
}

# Run Step 5 non-fatally — never let an optional hardening step roll back the install.
if ! step5_branch_rulesets; then
  warning "Step 5 (branch rulesets) encountered an error — continuing with installation"
  info "To configure manually after install: $LOOM_ROOT/scripts/install/setup-branch-protection.sh $TARGET_PATH"
fi

echo ""

# ============================================================================
# STEP 5b: Configure Repository Settings
# ============================================================================
# Optional post-install configuration — wrapped in a non-fatal helper for the
# same reason as Step 5: an interactive prompt on non-TTY stdin (or a missing
# admin permission) must not roll back the entire installation.
CURRENT_STEP="Configure Repository Settings"
header "Step 5b: Configure Repository Settings"
echo ""

step5b_repository_settings() {
  if [[ "$NON_INTERACTIVE" == "true" ]]; then
    # In non-interactive mode, apply repository settings automatically
    # This is needed for auto-merge to work on the installation PR
    info "Applying repository settings (required for auto-merge)..."
    if "$LOOM_ROOT/scripts/install/setup-repository-settings.sh" "$TARGET_PATH"; then
      echo ""
    else
      echo ""
      warning "Failed to configure repository settings (may require admin permissions)"
      info "Auto-merge may not be available for the installation PR"
    fi
  else
    echo ""
    local REPLY
    read -p "Configure repository merge and auto-merge settings? (y/N) " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Yy]$ ]]; then
      info "Applying repository settings..."
      if "$LOOM_ROOT/scripts/install/setup-repository-settings.sh" "$TARGET_PATH"; then
        echo ""
      else
        echo ""
        warning "Failed to configure repository settings (may require admin permissions)"
        info "You can configure manually via GitHub Settings > General"
      fi
    else
      info "Skipping repository settings"
      info "To configure later, run: $LOOM_ROOT/scripts/install/setup-repository-settings.sh $TARGET_PATH"
    fi
  fi
}

# Run Step 5b non-fatally — see comment on Step 5 above.
if ! step5b_repository_settings; then
  warning "Step 5b (repository settings) encountered an error — continuing with installation"
  info "To configure manually after install: $LOOM_ROOT/scripts/install/setup-repository-settings.sh $TARGET_PATH"
fi

echo ""

# ============================================================================
# STEP 6: Create Pull Request
# ============================================================================
CURRENT_STEP="Create PR"
header "Step 6: Creating Pull Request"
echo ""

if [[ ! -x "$LOOM_ROOT/scripts/install/create-pr.sh" ]]; then
  error "Installation script not found: create-pr.sh"
fi

PR_URL_RAW=$("$LOOM_ROOT/scripts/install/create-pr.sh" "$TARGET_PATH/$WORKTREE_PATH" "$BASE_BRANCH") || {
  # create-pr.sh prints detailed error info to stderr (visible above)
  error "Failed to create pull request (see details above)"
}

# Check if installation was already complete (no changes needed)
# IMPORTANT: Check this BEFORE trying to parse as URL
if [[ "$PR_URL_RAW" == *"NO_CHANGES_NEEDED"* ]]; then
  info "Loom is already installed - cleaning up..."

  # Remove the worktree
  cd "$TARGET_PATH"
  git worktree remove "${WORKTREE_PATH}" --force 2>/dev/null || true
  git worktree prune 2>/dev/null || true
  rm -rf "${TARGET_PATH}/${WORKTREE_PATH}"
  git branch -D "${BRANCH_NAME}" 2>/dev/null || true

  # Restore any staged changes left by --clean uninstall
  # When --clean runs uninstall in --local mode, it stages file deletions
  # Since no changes are needed, we restore those files to their original state
  if [[ "$CLEAN_FIRST" == "true" ]]; then
    info "Restoring files staged by uninstall..."
    if ! git -C "$TARGET_PATH" restore --staged . 2>/dev/null; then
      warning "Failed to unstage changes - run 'git restore --staged .' in $TARGET_PATH"
    fi
    if ! git -C "$TARGET_PATH" checkout -- . 2>/dev/null; then
      warning "Failed to restore files - run 'git checkout -- .' in $TARGET_PATH"
    fi

    # Verify cleanup
    if ! git -C "$TARGET_PATH" diff --quiet 2>/dev/null || \
       ! git -C "$TARGET_PATH" diff --staged --quiet 2>/dev/null; then
      warning "Working tree still has changes after cleanup:"
      git -C "$TARGET_PATH" status --short 2>/dev/null || true
      warning "To fix: cd $TARGET_PATH && git restore --staged . && git checkout -- ."
    fi
  fi

  # Disable error trap and exit successfully
  trap - EXIT SIGINT SIGTERM

  echo ""
  success "Loom is already installed in this repository"
  echo ""
  info "No pull request was created because all files are up to date."
  echo ""
  exit 0
fi

# Parse output: PR_URL|MERGE_STATUS
# Extract the last line containing PR_URL|STATUS format
LAST_OUTPUT_LINE=$(echo "$PR_URL_RAW" | tail -1)
PR_URL=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f1)
MERGE_STATUS=$(echo "$LAST_OUTPUT_LINE" | cut -d'|' -f2)

# Validate PR URL - also try to extract from the raw output if parsing failed
if [[ ! "$PR_URL" =~ ^https:// ]]; then
  # Fallback: try to extract URL from anywhere in the output (GitHub or Gitea)
  PR_URL=$(echo "$PR_URL_RAW" | grep -oE 'https://[^[:space:]|]+/(pull|pulls)/[0-9]+' | head -1 | tr -d '[:space:]')
fi

if [[ ! "$PR_URL" =~ ^https:// ]]; then
  error "Invalid PR URL returned: $PR_URL"
fi

# Default merge status if not set
MERGE_STATUS="${MERGE_STATUS:-manual}"

success "Pull request created"
echo ""

# ============================================================================
# Post-Install Verification: Detect changes introduced by the installer
# ============================================================================
# We compare the current `git status --porcelain` against the snapshot taken
# at install start (PRE_INSTALL_STATUS). Only entries that did not exist in
# the pre-install snapshot are reported as install residue. Pre-existing
# dirty state in the user's working tree was already acknowledged earlier
# (validate-target.sh); we must not flag it again here, and we must never
# recommend destructive cleanup that would discard the user's work.
CURRENT_STEP="Verify Working Tree"
header "Verifying main working directory..."
echo ""

cd "$TARGET_PATH"

POST_INSTALL_STATUS=$(git status --porcelain 2>/dev/null || true)

# Compute install-introduced entries: lines present after install but not
# before. We use line-level set difference via grep -F -v -x against the
# pre-install snapshot. Empty snapshot is handled by treating all current
# entries as new (which is correct).
if [[ -z "$POST_INSTALL_STATUS" ]]; then
  INSTALL_RESIDUE=""
elif [[ -z "$PRE_INSTALL_STATUS" ]]; then
  INSTALL_RESIDUE="$POST_INSTALL_STATUS"
else
  INSTALL_RESIDUE=$(printf '%s\n' "$POST_INSTALL_STATUS" \
    | grep -F -x -v -f <(printf '%s\n' "$PRE_INSTALL_STATUS") \
    || true)
fi

if [[ -z "$INSTALL_RESIDUE" ]]; then
  success "Main working directory is clean (relative to pre-install state)"
else
  echo ""
  warning "Installer left changes in the main working directory:"
  printf '%s\n' "$INSTALL_RESIDUE"
  echo ""
  warning "These paths appear new since the installer started. Inspect them"
  warning "before cleaning up — do not blindly discard, as some may overlap"
  warning "with your own in-progress work."
  echo ""
  warning "To inspect:"
  warning "  cd $TARGET_PATH"
  warning "  git status"
  warning "  git diff <path>"
fi

echo ""

# ============================================================================
# Installation Complete
# ============================================================================
CURRENT_STEP="Complete"

# Disable error trap - we completed successfully
trap - EXIT SIGINT SIGTERM

echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║              ✓ Installation Complete!                    ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

success "Loom ${LOOM_VERSION} installed successfully"
echo ""

# Show PR status based on merge result
case "$MERGE_STATUS" in
  merged)
    info "✓ Pull Request: ${PR_URL} (merged)"
    ;;
  auto)
    info "⏳ Pull Request: ${PR_URL} (auto-merge enabled)"
    ;;
  *)
    info "📦 Pull Request: ${PR_URL}"
    ;;
esac
echo ""

header "What's Included:"
echo "  ✅ .loom/ directory with configuration and scripts"
echo "  ✅ .claude/ directory with slash commands"
echo "  ✅ .github/ directory with labels and issue templates"
echo "  ✅ CLAUDE.md documentation"
echo ""

header "Next Steps:"
case "$MERGE_STATUS" in
  merged)
    # PR was merged - ready to use immediately
    echo "  Loom is ready to use! Choose your workflow:"
    echo ""
    echo "  Manual Mode (recommended to start):"
    echo "    cd $TARGET_PATH && claude"
    echo "    Then use /builder, /judge, or other role commands"
    echo ""
    echo "  Daemon Mode (autonomous orchestration):"
    echo "    cd $TARGET_PATH && ./.loom/scripts/daemon.sh start"
    echo "    Then in Claude Code: /loom"
    ;;
  auto)
    # Auto-merge enabled - PR will merge once requirements are met
    echo "  The installation PR has auto-merge enabled and will merge"
    echo "  automatically once ruleset requirements are met."
    echo ""
    echo "  Once merged, choose your workflow:"
    echo ""
    echo "  Manual Mode (recommended to start):"
    echo "    cd $TARGET_PATH && claude"
    echo "    Then use /builder, /judge, or other role commands"
    echo ""
    echo "  Daemon Mode (autonomous orchestration):"
    echo "    cd $TARGET_PATH && ./.loom/scripts/daemon.sh start"
    echo "    Then in Claude Code: /loom"
    ;;
  *)
    # Manual merge required
    echo "  1. Review and merge the pull request: ${PR_URL}"
    echo "  2. Choose your workflow:"
    echo "     Manual Mode (recommended to start):"
    echo "       cd $TARGET_PATH && claude"
    echo "       Then use /builder, /judge, or other role commands"
    echo "     Daemon Mode (autonomous orchestration):"
    echo "       cd $TARGET_PATH && ./.loom/scripts/daemon.sh start"
    echo "       Then in Claude Code: /loom"
    ;;
esac
echo ""

# ---------------------------------------------------------------------------
# CI integration hint (issue #3333).
#
# The install PR carries passive markers (chore(loom): title prefix,
# `loom-install: true` body line, `Skip-CI-Hint: docs-only` commit trailer)
# that target repos can detect via path-ignore, title filters, or body grep
# to skip expensive CI on docs-only install PRs. Point operators at the
# integration doc so they know what's available.
# ---------------------------------------------------------------------------
header "Optional: Skip CI on this and future install PRs"
echo "  The install PR includes passive markers (title prefix, body line,"
echo "  commit trailer) detectable by opt-in CI filters. To skip CI for"
echo "  Loom install/update PRs, add a paths-ignore block to the target's"
echo "  workflows:"
echo ""
echo "      on:"
echo "        pull_request:"
echo "          paths-ignore:"
echo "            - '.loom/**'"
echo "            - '.claude/**'"
echo "            - '.codex/**'"
echo "            - 'CLAUDE.md'"
echo "            - '.github/labels.yml'"
echo ""
echo "  Full integration guide: ${TARGET_PATH}/.loom/docs/ci-integration.md"
if [[ "$SKIP_TARGET_CI" == "true" ]]; then
  echo ""
  echo "  Note: --skip-target-ci was set — this install PR carries [skip ci]"
  echo "  and target CI workflows were skipped via the universal directive."
fi
echo ""

info "See CLAUDE.md in the target repository for complete usage details."
