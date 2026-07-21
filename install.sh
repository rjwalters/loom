#!/usr/bin/env bash
# Loom Setup - Install Loom into a target repository
# Usage: ./install.sh [OPTIONS] [/path/to/target-repo]
#
# Options:
#   -y, --yes                  Non-interactive mode (skip confirmation prompts)
#   --quick                    Quick Install - direct install without GitHub workflow
#   --full                     Full Install - creates issue, worktree, and PR
#   --allow-non-main-source    Permit installing from a non-main / detached-HEAD Loom source
#                              (forwarded to scripts/install-loom.sh)
#   --allow-stale-target       Permit installing over a target whose Loom is newer/stale
#                              (forwarded to scripts/install-loom.sh)
#   -h, --help                 Show this help message
#
# Examples:
#   ./install.sh --quick ~/projects/my-app
#   ./install.sh --full /path/to/team-project
#   ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick

set -euo pipefail

# Handle Ctrl-C and SIGTERM during interactive prompts
trap 'echo ""; echo -e "\033[0;34mℹ Installation cancelled\033[0m"; exit 130' SIGINT
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

# Install hooks and CLI wrapper that loom-daemon init doesn't handle.
#
# Issue #3625: an existing hook may be a downstream-tuned or forked copy — most
# notably a customized guard-destructive.sh with a hand-tuned rm allowlist — so
# it must NOT be silently clobbered on the quick-install/update path. Preserve
# any existing .loom/hooks/<name> unless an explicit force overwrite is
# requested (the caller passes "true", e.g. behind --clean). This mirrors the
# preserve-unless-force behavior already in scripts/install-loom.sh:1099-1116,
# which the quick path previously diverged from with an unconditional cp.
install_hooks_and_cli() {
  local loom_root="$1"
  local target="$2"
  local force="${3:-false}"

  # Install hooks
  if [[ -d "$loom_root/defaults/hooks" ]]; then
    mkdir -p "$target/.loom/hooks"
    for hook_file in "$loom_root/defaults/hooks/"*.sh; do
      [[ -f "$hook_file" ]] || continue
      hook_name=$(basename "$hook_file")
      if [[ -f "$target/.loom/hooks/$hook_name" ]] && [[ "$force" != "true" ]]; then
        warning "Preserving existing hook: $hook_name (use --clean to overwrite)"
      else
        cp "$hook_file" "$target/.loom/hooks/$hook_name"
        chmod +x "$target/.loom/hooks/$hook_name"
        success "Installed hook: $hook_name"
      fi
    done
  fi

  # Install CLI wrapper
  if [[ -f "$loom_root/defaults/.loom/bin/loom" ]]; then
    mkdir -p "$target/.loom/bin"
    cp "$loom_root/defaults/.loom/bin/loom" "$target/.loom/bin/loom"
    chmod +x "$target/.loom/bin/loom"
    success "Installed .loom/bin/loom CLI"
  fi

  # Install loom.sh convenience wrapper at repo root
  if [[ -f "$loom_root/defaults/loom.sh" ]]; then
    cp "$loom_root/defaults/loom.sh" "$target/loom.sh"
    chmod +x "$target/loom.sh"
    success "Installed loom.sh"
  fi
}

# Export LOOM_VERSION and LOOM_COMMIT so `loom-daemon init`'s template
# substitution can fill {{LOOM_VERSION}} / {{LOOM_COMMIT}} placeholders in
# the CLAUDE.md templates instead of falling back to the literal string
# "unknown" (see loom-daemon/src/init/templates.rs:49-50). Issue #3502.
#
# Mirrors the env-export pattern from scripts/install-loom.sh:710,723.
prepare_loom_metadata_env() {
  local loom_root="$1"

  if [[ ! -f "$loom_root/package.json" ]]; then
    warning "Cannot find package.json in $loom_root — LOOM_VERSION will be 'unknown'"
    return 0
  fi

  LOOM_VERSION=$(node -pe "require('$loom_root/package.json').version" 2>/dev/null) || {
    warning "Failed to extract version from package.json — LOOM_VERSION will be 'unknown'"
    return 0
  }

  LOOM_COMMIT=$(git -C "$loom_root" rev-parse --short HEAD 2>/dev/null) || {
    warning "Failed to get git commit hash — LOOM_COMMIT will be 'unknown'"
    LOOM_COMMIT=""
  }

  export LOOM_VERSION
  export LOOM_COMMIT
}

# Post-`loom-daemon init` artifacts that loom-daemon does not write itself.
# Invoked by both the `--quick` reinstall branch and the fresh `--quick`
# install case so neither path drops:
#   - .loom/config/skill-routes.json (port of scripts/install-loom.sh:1032-1048)
#   - .loom/loom-source-path        (port of scripts/install-loom.sh:1067-1074)
#   - .loom/install-metadata.json   (port of scripts/install-loom.sh:1261-1270)
#
# See issue #3502. Note: setup-python-tools.sh is intentionally NOT invoked
# from --quick — Python tooling is out of scope for the fast install path.
finalize_quick_install() {
  local loom_root="$1"
  local target="$2"

  # 1. Copy default config files (skill-routes.json template, etc.).
  if [[ -d "$loom_root/defaults/config" ]]; then
    mkdir -p "$target/.loom/config"
    for config_file in "$loom_root/defaults/config/"*.json; do
      [[ -f "$config_file" ]] || continue
      local config_name
      config_name=$(basename "$config_file")
      if [[ -f "$target/.loom/config/$config_name" ]]; then
        info "Skipping existing config: $config_name"
      else
        cp "$config_file" "$target/.loom/config/$config_name"
        success "Installed config: $config_name"
      fi
    done
  fi

  # 2. Record Loom source path (consumed by agent-metrics.sh and other
  # wrapper scripts to locate loom-tools/ in the source checkout).
  echo "$loom_root" > "$target/.loom/loom-source-path"
  success "Recorded Loom source path"

  # 3. Write install-metadata.json with the same schema as the legacy
  # installer so uninstall-loom.sh and install-loom.sh's upgrade detector
  # can both consume it.
  local installed_files_json="[]"
  if [[ -f "$loom_root/scripts/install/manifest.sh" ]]; then
    # shellcheck source=/dev/null
    LOOM_ROOT="$loom_root" TARGET_PATH="$target" \
      source "$loom_root/scripts/install/manifest.sh"
    installed_files_json="$(LOOM_ROOT="$loom_root" TARGET_PATH="$target" \
      _emit_installed_files_manifest)"
  else
    warning "manifest.sh not found — install-metadata.json will have empty installed_files"
  fi

  local install_date
  install_date="$(date +%Y-%m-%d)"

  cat > "$target/.loom/install-metadata.json" <<METADATA
{
  "loom_version": "${LOOM_VERSION:-unknown}",
  "loom_commit": "${LOOM_COMMIT:-unknown}",
  "install_date": "${install_date}",
  "loom_source": "${loom_root}",
  "installed_files": ${installed_files_json}
}
METADATA
  success "Recorded installation metadata"

  # Quick Install ships .github/labels.yml but does NOT create the labels on
  # the forge (that is a Full Install step). Point the operator at the shipped
  # sync script so the label-based workflow doesn't break on first use (#3582).
  info "Labels not yet synced. Run '.loom/scripts/sync-labels.sh' from the"
  info "  repo root to create the Loom workflow labels on the forge (or use"
  info "  Full Install, which syncs them automatically)."
}

# Verify critical installation files exist
verify_install() {
  local target="$1"
  local critical_files=(
    ".loom/config.json"
    ".loom/scripts/worktree.sh"
    ".loom/scripts/lib/loom-tools.sh"
    ".loom/install-metadata.json"
    ".loom/config/skill-routes.json"
  )
  local missing=0
  for file in "${critical_files[@]}"; do
    if [[ ! -f "$target/$file" ]]; then
      warning "Missing critical file: $file"
      missing=$((missing + 1))
    fi
  done

  # Defense-in-depth: surface any unsubstituted {{LOOM_VERSION}} /
  # {{INSTALL_DATE}} survivors in .loom/CLAUDE.md (issue #3502). Also
  # surface the literal "unknown" version line, which means the daemon's
  # substituter ran but LOOM_VERSION was not exported before invocation.
  local claude_md="$target/.loom/CLAUDE.md"
  if [[ -f "$claude_md" ]]; then
    if grep -q '{{LOOM_VERSION}}\|{{LOOM_COMMIT}}\|{{INSTALL_DATE}}\|{{REPO_OWNER}}\|{{REPO_NAME}}' "$claude_md"; then
      warning "Unsubstituted template placeholder(s) found in .loom/CLAUDE.md"
      missing=$((missing + 1))
    fi
    if grep -Eq '^\*\*Loom Version\*\*:[[:space:]]+unknown' "$claude_md"; then
      warning ".loom/CLAUDE.md has 'Loom Version: unknown' — LOOM_VERSION was not exported before loom-daemon init"
      missing=$((missing + 1))
    fi
  fi

  if [[ $missing -gt 0 ]]; then
    warning "$missing critical file(s) missing or corrupted after installation"
  fi
}

# Issue #3588: re-append the current Loom ephemeral .gitignore patterns after a
# --quick reinstall stash pop that was performed against a HEAD-reset .gitignore.
#
# The reinstall restores .gitignore to its committed HEAD state before popping so
# the user's stashed hunk applies cleanly (see the pop block below). That reset
# strips the Loom patterns the daemon's `init` had (re-)written, so we re-apply
# them here. The pattern list is derived from the post-init snapshot (lines that
# were present there but absent from the committed HEAD version) rather than
# hard-coded, so it never drifts from the daemon's authoritative list in
# loom-daemon/src/init/post_init.rs. Appending only missing lines keeps this
# idempotent (append-only), mirroring `update_gitignore`.
reapply_loom_gitignore_patterns() {
  local target_path="$1"
  local postinit_snapshot="$2"
  local gitignore="$target_path/.gitignore"

  [[ -f "$postinit_snapshot" && -f "$gitignore" ]] || return 0

  # The committed .gitignore (the stash base the user's hunk was recorded
  # against). Lines present in the post-init snapshot but not here are exactly
  # the Loom patterns `init` (re-)appended.
  local head_version loom_lines
  head_version="$(git -C "$target_path" show HEAD:.gitignore 2>/dev/null)"

  loom_lines="$(grep -vxF -f <(printf '%s\n' "$head_version") "$postinit_snapshot" 2>/dev/null || true)"
  [[ -z "$loom_lines" ]] && return 0

  local line
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    if ! grep -qxF -- "$line" "$gitignore" 2>/dev/null; then
      # Ensure a trailing newline before appending (command substitution strips
      # the newline, so a non-empty result means the file did NOT end in \n).
      if [[ -s "$gitignore" && -n "$(tail -c1 "$gitignore" 2>/dev/null)" ]]; then
        printf '\n' >>"$gitignore"
      fi
      printf '%s\n' "$line" >>"$gitignore"
    fi
  done <<<"$loom_lines"
}

# Issue #3663: re-apply the current Loom-owned CLAUDE.md marker block after a
# --quick reinstall stash pop that was performed against a HEAD-reset CLAUDE.md.
#
# This generalizes the #3588 .gitignore treatment (HEAD-reset-before-pop, then
# reapply) to CLAUDE.md, whose Loom content is a marker-delimited block
# (`<!-- BEGIN LOOM ORCHESTRATION -->` … `<!-- END LOOM ORCHESTRATION -->`)
# rather than a set of appended lines. The reinstall restores CLAUDE.md to its
# committed HEAD state before popping so the user's stashed hunk applies cleanly
# (see the pop block below). That reset reverts the Loom block to its old
# committed content, so we splice the freshly written block back in here.
#
# We rebuild the file as: everything BEFORE the begin marker (from the popped
# working copy — the user's restored content) + the block (begin…end inclusive)
# taken from the post-init snapshot (the daemon's authoritative fresh block) +
# everything AFTER the end marker (again from the popped working copy). Only the
# delimited Loom region is replaced; every line the user's pop restored outside
# the markers (e.g. their own `REPO-SKILLS` block) is left byte-for-byte intact.
# Derived from the snapshot, never hard-coded, mirroring
# reapply_loom_gitignore_patterns's "trust the daemon's output" property.
# Idempotent: splicing an already-current block is a no-op. If either file lacks
# both markers there is no delimited region to reconcile, so the popped file is
# left as-is.
reapply_loom_claude_md_block() {
  local target_path="$1"
  local postinit_snapshot="$2"
  local claude_md="$target_path/CLAUDE.md"
  local begin="<!-- BEGIN LOOM ORCHESTRATION -->"
  local end="<!-- END LOOM ORCHESTRATION -->"

  [[ -f "$postinit_snapshot" && -f "$claude_md" ]] || return 0

  # Both the snapshot and the popped file must carry the marker block, else
  # there is no delimited Loom region to splice — leave the popped file alone.
  grep -qF "$begin" "$postinit_snapshot" && grep -qF "$end" "$postinit_snapshot" || return 0
  grep -qF "$begin" "$claude_md" && grep -qF "$end" "$claude_md" || return 0

  local tmp
  tmp="$(mktemp 2>/dev/null || true)"
  [[ -n "$tmp" ]] || return 0

  # Pass 1 (snapshot): capture the begin…end block into `block`.
  # Pass 2 (popped file): print user content up to begin, emit the captured
  # block once, then resume printing user content after end.
  if awk -v b="$begin" -v e="$end" '
    FNR==NR {
      if (index($0, b)) grab=1
      if (grab) block = block $0 ORS
      if (index($0, e)) grab=0
      next
    }
    {
      if (index($0, b)) { printf "%s", block; skip=1 }
      if (!skip) print
      if (index($0, e)) skip=0
    }
  ' "$postinit_snapshot" "$claude_md" >"$tmp" 2>/dev/null && [[ -s "$tmp" ]]; then
    cat "$tmp" >"$claude_md" 2>/dev/null || true
  fi
  rm -f "$tmp" 2>/dev/null || true
  return 0
}

# Issue #3663: emit the Loom marker block (begin…end inclusive) from stdin.
# Used to decide whether a user's stashed CLAUDE.md edit lands INSIDE the Loom
# block (in which case a HEAD-reset+reapply would clobber it) or entirely
# outside it (safe to reset+reapply). Empty output means no block was found.
_emit_loom_claude_block() {
  awk '
    index($0, "<!-- BEGIN LOOM ORCHESTRATION -->") { inblk=1 }
    inblk { print }
    index($0, "<!-- END LOOM ORCHESTRATION -->") { inblk=0 }
  '
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

# Parse flags
NON_INTERACTIVE=false
INSTALL_TYPE=""
# Source/target override flags accepted by scripts/install-loom.sh. The top-level
# wrapper does not act on them (its source guard runs only in the delegated
# installer), but it must accept and forward them so the flags it suggests
# actually work. See issue #3650.
SOURCE_OVERRIDE_FLAGS=()
while [[ "${1:-}" == -* ]]; do
  case "$1" in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    --quick)
      # Check for conflicting --full flag
      if [[ "$INSTALL_TYPE" == "2" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="1"
      NON_INTERACTIVE=true  # --quick implies non-interactive
      shift
      ;;
    --full)
      # Check for conflicting --quick flag
      if [[ "$INSTALL_TYPE" == "1" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="2"
      NON_INTERACTIVE=true  # --full implies non-interactive
      shift
      ;;
    --allow-non-main-source|--allow-stale-target)
      # Pass-through: accepted here so the wrapper's own suggestion works, then
      # forwarded to scripts/install-loom.sh at the Full-Install delegation execs.
      SOURCE_OVERRIDE_FLAGS+=("$1")
      shift
      ;;
    -h|--help)
      echo "Usage: ./install.sh [OPTIONS] [TARGET_PATH]"
      echo ""
      echo "Options:"
      echo "  -y, --yes                  Non-interactive mode (skip confirmation prompts)"
      echo "  --quick                    Quick Install - direct install without GitHub workflow"
      echo "  --full                     Full Install - creates issue, worktree, and PR"
      echo "  --allow-non-main-source    Permit installing from a non-main / detached-HEAD"
      echo "                             Loom source (forwarded to scripts/install-loom.sh)"
      echo "  --allow-stale-target       Permit installing over a newer/stale target"
      echo "                             (forwarded to scripts/install-loom.sh)"
      echo "  -h, --help                 Show this help message"
      echo ""
      echo "Examples:"
      echo "  ./install.sh --quick ~/projects/my-app"
      echo "  ./install.sh --full /path/to/team-project"
      echo "  ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick install"
      echo "  ./install.sh --yes --allow-non-main-source /path/to/target  # Install from a non-main source"
      exit 0
      ;;
    *)
      error "Unknown flag: $1"
      ;;
  esac
done

# Early validation for --full: requires gh CLI (for GitHub repos)
# Gitea repos use the API directly and don't need gh CLI
if [[ "$INSTALL_TYPE" == "2" ]] && ! command -v gh &> /dev/null; then
  warning "GitHub CLI (gh) not found. Required for GitHub repos.\n       Install: brew install gh\n       For Gitea repos, set GITEA_TOKEN instead.\n       Or use --quick for installation without forge integration"
fi

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

# Check if it's a git repository (worktree-safe: a linked worktree's .git is a file)
if ! git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  warning "$TARGET_PATH is not a git repository."
  echo ""
  echo "Would you like to initialize git and optionally set up GitHub?"
  echo ""
  echo "This will:"
  echo "  1. Run 'git init' in the directory"
  echo "  2. Create a sensible .gitignore file"
  echo "  3. Create an initial commit"
  echo "  4. Optionally create a GitHub repository and set up remote"
  echo ""
  read -r -p "Initialize git repository? [y/N] " -n 1 INIT_GIT
  echo ""

  if [[ ! $INIT_GIT =~ ^[Yy]$ ]]; then
    error "Cannot proceed without a git repository.\n       Run 'git init' manually or choose a different directory."
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

# Loom (will be added by installation)
# .loom/state.json
# .loom/worktrees/
# .loom/*.log
GITIGNORE
    success "Created .gitignore"
  fi

  # Create initial commit
  info "Creating initial commit..."
  git add -A
  git commit -m "Initial commit" --quiet || error "Failed to create initial commit"
  success "Initial commit created"
  echo ""

  # Offer remote repository creation
  if command -v gh &> /dev/null; then
    echo "Would you like to create a GitHub repository for this project?"
    echo "(For Gitea, create the repository manually and add the remote)"
    echo ""
    read -r -p "Create GitHub repository? [y/N] " -n 1 CREATE_REPO
    echo ""

    if [[ $CREATE_REPO =~ ^[Yy]$ ]]; then
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
        success "Repository created and pushed"
      else
        warning "Failed to create repository. Continuing with local git only."
        info "You can create the repository later with: gh repo create"
      fi
      echo ""
    fi
  else
    info "GitHub CLI (gh) not found - skipping remote repository creation"
    info "For GitHub: install gh CLI (brew install gh)"
    info "For Gitea: create the repo manually and run: git remote add origin <url>"
    echo ""
  fi
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

# Check for GitHub CLI (optional, needed for Full Install with GitHub repos)
if command -v gh &> /dev/null; then
  success "gh: $(gh --version | head -1)"
else
  warning "gh (GitHub CLI) not found - needed for Full Install with GitHub repos"
  info "  Install with: brew install gh"
  info "  For Gitea repos, gh is not required (set GITEA_TOKEN instead)"
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

# Check if this is the Loom source repository (self-installation)
is_loom_source_repo() {
  local path="$1"
  # Check for marker file
  [[ -f "$path/.loom-source" ]] && return 0
  # Check for Loom-specific directory structure
  [[ -d "$path/loom-daemon" && -d "$path/loom-api" && -d "$path/defaults" ]] && return 0
  return 1
}

if is_loom_source_repo "$TARGET_PATH"; then
  echo ""
  header "╔═══════════════════════════════════════════════════════════╗"
  header "║              Loom Source Repository Detected              ║"
  header "╚═══════════════════════════════════════════════════════════╝"
  echo ""
  info "This appears to be the Loom source repository itself."
  info "Self-installation runs in validation-only mode to prevent data loss."
  echo ""
  info "The Loom repo's .loom/ directory IS the source of truth for defaults."
  info "Installing would overwrite rich content with minimal templates."
  echo ""
  read -r -p "Run validation to check configuration? [Y/n] " -n 1 VALIDATE_REPLY
  echo ""
  if [[ $VALIDATE_REPLY =~ ^[Nn]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
  FORCE_FLAG=""
  SELF_INSTALL=true
elif [[ -d "$TARGET_PATH/.loom" ]]; then
  warning "Loom appears to be already installed in this repository"
  echo ""
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    info "Reinstall will uninstall the existing installation first, then perform"
    info "a fresh Quick Install."
  else
    info "Reinstall will uninstall the existing installation first, then perform"
    info "a fresh install and create a PR with the changes."
  fi
  echo ""

  if [[ "$NON_INTERACTIVE" != true ]]; then
    read -r -p "Proceed with reinstall? [y/N] " -n 1 REINSTALL_CONFIRM
    echo ""
    if [[ ! $REINSTALL_CONFIRM =~ ^[Yy]$ ]]; then
      info "Installation cancelled"
      exit 0
    fi
  else
    info "Non-interactive mode: proceeding with reinstall"
  fi

  # Issue #3545: for a --quick reinstall, guard uncommitted user changes across
  # the uninstall→reinstall cycle (mirrors the stash guard in the sibling
  # scripts/install-loom.sh --clean path). The uninstall runs `git add` in the
  # target tree and the reinstall reconciles the index afterwards; stashing
  # first keeps a user's pre-existing staged/working changes from being caught
  # up in either step. The non-quick reinstall delegates to install-loom.sh,
  # which performs its own guarding, so it is intentionally left unstashed here.
  #
  # Issue #3597: scope the stash to Loom-owned paths. The original unscoped
  # `git stash push` swept sibling installers' uncommitted tracked changes
  # (.anvil/*, .claude/skills/repo/*, non-Loom CLAUDE.md sections, …) into the
  # stash and left a half-old/half-new hybrid tree. Restrict the stash to the
  # dirty ∩ (Loom ownership set + .gitignore) intersection so sibling changes
  # are never touched. Empty intersection → no stash at all.
  REINSTALL_STASHED_USER_CHANGES=false
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    # shellcheck source=scripts/install/stash-scope.sh
    source "$LOOM_ROOT/scripts/install/stash-scope.sh"
    REINSTALL_OWNED_DIRTY=()
    while IFS= read -r _owned_path; do
      [[ -n "$_owned_path" ]] && REINSTALL_OWNED_DIRTY+=("$_owned_path")
    done < <(_emit_loom_owned_dirty_paths "$LOOM_ROOT" "$TARGET_PATH")

    if [[ ${#REINSTALL_OWNED_DIRTY[@]} -gt 0 ]]; then
      info "Stashing uncommitted Loom-owned changes before reinstall..."
      if git -C "$TARGET_PATH" stash push \
           -m "loom-install: preserving user changes before --quick reinstall" \
           -- "${REINSTALL_OWNED_DIRTY[@]}" 2>/dev/null; then
        REINSTALL_STASHED_USER_CHANGES=true
        REINSTALL_STASH_REF="$(git -C "$TARGET_PATH" stash list 2>/dev/null | head -1)"
        success "Loom-owned changes stashed → ${REINSTALL_STASH_REF:-stash@{0}}"
        info "  Stashed ${#REINSTALL_OWNED_DIRTY[@]} Loom-owned path(s): ${REINSTALL_OWNED_DIRTY[*]}"
        info "  Recover manually with: git -C \"$TARGET_PATH\" stash pop"
      else
        warning "Failed to stash user changes - continuing without stash"
        warning "Uncommitted changes may appear alongside the reinstall diff"
      fi
    fi
  fi

  # Issue #3598: snapshot the committed .loom/config.json before the chained
  # uninstall deletes it. `config.json` is listed in uninstall-loom.sh's
  # RUNTIME_ARTIFACTS and is removed from disk, but it is consumer configuration
  # (e.g. a load-bearing `worktree.root` override), not a runtime artifact.
  # Restoring the snapshot before `loom-daemon init` (below) lets the daemon's
  # merge-aware config copy preserve consumer keys instead of regenerating the
  # file from the template. Mirrors the #3588 .gitignore snapshot pattern.
  # Standalone uninstall behavior is intentionally unchanged.
  REINSTALL_CONFIG_SNAPSHOT=""
  if [[ -f "$TARGET_PATH/.loom/config.json" ]]; then
    REINSTALL_CONFIG_SNAPSHOT="$(mktemp 2>/dev/null || true)"
    if [[ -n "$REINSTALL_CONFIG_SNAPSHOT" ]]; then
      cp "$TARGET_PATH/.loom/config.json" "$REINSTALL_CONFIG_SNAPSHOT" 2>/dev/null || \
        REINSTALL_CONFIG_SNAPSHOT=""
    fi
  fi

  # Uninstall existing installation (local mode, no separate PR)
  info "Uninstalling existing Loom installation..."
  "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
    error "Uninstall failed - aborting reinstall"
  echo ""
  success "Existing installation removed"
  echo ""

  # If --quick was specified, do a quick reinstall instead of full workflow
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    info "Running fresh Quick Install..."
    echo ""

    # Check if loom-daemon is built
    if [[ ! -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
      warning "loom-daemon binary not found"
      info "Building loom-daemon (this may take a minute)..."
      cd "$LOOM_ROOT"
      pnpm daemon:build || error "Failed to build loom-daemon"
      echo ""
    fi

    # Export LOOM_VERSION / LOOM_COMMIT so the daemon's template substituter
    # fills CLAUDE.md correctly (issue #3502).
    prepare_loom_metadata_env "$LOOM_ROOT"

    # Issue #3598: restore the snapshotted config.json before init so the
    # daemon's merge-aware config copy sees the consumer's committed values
    # (e.g. worktree.root) and preserves them in the merged result.
    if [[ -n "$REINSTALL_CONFIG_SNAPSHOT" && -f "$REINSTALL_CONFIG_SNAPSHOT" ]]; then
      mkdir -p "$TARGET_PATH/.loom"
      cp "$REINSTALL_CONFIG_SNAPSHOT" "$TARGET_PATH/.loom/config.json" 2>/dev/null || true
    fi

    # Run loom-daemon init
    "$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
      error "Installation failed"

    # Clean up the config snapshot now that init has merged it into place.
    [[ -n "$REINSTALL_CONFIG_SNAPSHOT" ]] && rm -f "$REINSTALL_CONFIG_SNAPSHOT" 2>/dev/null || true

    # Install hooks and CLI wrapper (not handled by loom-daemon init)
    install_hooks_and_cli "$LOOM_ROOT" "$TARGET_PATH"
    # Emit skill-routes.json, install-metadata.json, loom-source-path (#3502).
    finalize_quick_install "$LOOM_ROOT" "$TARGET_PATH"
    verify_install "$TARGET_PATH"

    # Issue #3545: reconcile the git index after the uninstall→reinstall cycle.
    # The chained uninstall staged the deletion of every prior Loom file (now
    # scoped to Loom-managed paths — see scripts/uninstall-loom.sh), then
    # `loom-daemon init --force` rewrote those files to disk WITHOUT touching
    # the index. Left as-is, `git status` shows ~150 paired staged-`D` /
    # untracked-`??` entries instead of the real version-upgrade diff. Unstage
    # the uninstall's staged deletions so the working tree reflects only the
    # actual old→new file changes.
    #
    # Issue #3597: scope the unstage to Loom-owned paths so user-staged
    # non-Loom changes (sibling installers, unrelated work) stay staged. The
    # uninstall only stages Loom-managed paths (#3450), so the dirty ∩
    # ownership intersection is exactly the set of staged deletions to undo.
    info "Reconciling git index after reinstall..."
    RECONCILE_PATHS=()
    while IFS= read -r _owned_path; do
      [[ -n "$_owned_path" ]] && RECONCILE_PATHS+=("$_owned_path")
    done < <(_emit_loom_owned_dirty_paths "$LOOM_ROOT" "$TARGET_PATH")
    if [[ ${#RECONCILE_PATHS[@]} -gt 0 ]]; then
      git -C "$TARGET_PATH" restore --staged -- "${RECONCILE_PATHS[@]}" 2>/dev/null || \
        git -C "$TARGET_PATH" reset -q HEAD -- "${RECONCILE_PATHS[@]}" 2>/dev/null || true
    fi

    # Issue #3611: reconcile GENERATED install-time artifacts that the ownership-
    # scoped pass above misses. `.loom/install-metadata.json` is written by
    # finalize_quick_install, NOT shipped in defaults/, so it is absent from the
    # manifest-derived ownership set that scopes RECONCILE_PATHS. The chained
    # uninstall staged its deletion (uninstall-loom.sh REMOVE_FILES → git add -A),
    # and finalize then rewrote it on disk as an UNTRACKED file — leaving a
    # `D` staged-deletion + `??` untracked pair. Committed as-is, that untracks
    # the very file verify_install and the upgrade detector depend on. Explicitly
    # unstage the staged deletion so the rewritten file reappears as a tracked
    # modification (` M`), never `D`+`??`. Guarded by a staged-diff check so it is
    # a no-op when the file was never staged for deletion. (`.loom/loom-source-path`
    # has the same generated-at-install shape but is gitignored → untracked → no
    # staged deletion, so it needs no reconcile; `.loom/config/skill-routes.json`
    # ships in defaults/config and is already covered by RECONCILE_PATHS.)
    for _generated_tracked in ".loom/install-metadata.json"; do
      if git -C "$TARGET_PATH" diff --staged --name-only -- "$_generated_tracked" 2>/dev/null \
           | grep -qxF "$_generated_tracked"; then
        git -C "$TARGET_PATH" restore --staged -- "$_generated_tracked" 2>/dev/null || \
          git -C "$TARGET_PATH" reset -q HEAD -- "$_generated_tracked" 2>/dev/null || true
      fi
    done

    # Restore any user changes stashed before the uninstall (see above).
    #
    # Issue #3588: the uninstall→init round-trip rewrites .gitignore
    # non-reversibly — the uninstall strips Loom patterns from mid-block and
    # collapses blank lines (scripts/uninstall-loom.sh), then `init` re-appends
    # the patterns at end-of-file (loom-daemon update_gitignore). That moves
    # lines relative to HEAD, so a stashed .gitignore hunk — recorded against
    # the committed context — no longer has a matching 3-way base on disk and
    # `git stash pop` conflicts. Previously the pop was silenced with
    # `2>/dev/null`: the conflict was hidden, the stash silently kept, and the
    # user's uncommitted .gitignore edit stranded (data-loss risk).
    #
    # Fix: before popping, restore .gitignore to its committed HEAD state so the
    # pop's 3-way base matches the stash base and the user's hunk applies
    # cleanly; then re-append the current Loom ephemeral patterns (append-only,
    # idempotent). If the pop still fails for any reason, surface the real
    # conflict output and a working recovery path instead of hiding it.
    if [[ "$REINSTALL_STASHED_USER_CHANGES" == "true" ]]; then
      info "Restoring stashed user changes..."

      # Issue #3663: generalize the #3588 .gitignore HEAD-reset-then-reapply to
      # every Loom-owned dirty file that carries a well-defined Loom-vs-user
      # split — today `.gitignore` (Loom patterns appended at EOF) and
      # `CLAUDE.md` (a marker-delimited Loom block with user content around it).
      # For each such path tracked at HEAD, snapshot the post-init on-disk
      # version (which carries the freshly written Loom content) and reset the
      # working copy to HEAD so the pop's 3-way base lines up with the committed
      # context and the user's stashed hunk applies cleanly. After a successful
      # pop we re-apply only the Loom portion from the snapshot (append for
      # `.gitignore`, marker-block splice for `CLAUDE.md`), leaving everything
      # the user's pop restored untouched.
      #
      # HEAD-reset is deliberately scoped to files with a reapply strategy. A
      # fully Loom-owned file (a role `.md`, `config.json`) has no partial
      # reapply, so resetting it would silently drop the reinstall's update —
      # those fall through to a plain pop, which surfaces a genuine conflict
      # (named below) instead of resetting-and-losing. Untracked/newly created
      # files have no HEAD base to restore to and the plain pop already applies
      # them cleanly, so they are skipped too.
      REINSTALL_RESET_PATHS=()
      REINSTALL_RESET_SNAPSHOTS=()
      REINSTALL_RESET_STRATEGIES=()
      for _owned_path in ${REINSTALL_OWNED_DIRTY[@]+"${REINSTALL_OWNED_DIRTY[@]}"}; do
        _reset_strategy=""
        case "$_owned_path" in
          .gitignore) _reset_strategy="gitignore" ;;
          CLAUDE.md)  _reset_strategy="claude_md" ;;
          *) continue ;;
        esac
        git -C "$TARGET_PATH" cat-file -e "HEAD:$_owned_path" 2>/dev/null || continue

        # Issue #3663: CLAUDE.md's reapply replaces the ENTIRE marker block, so
        # the reset+reapply path is only safe when (a) HEAD already carries a
        # Loom block to splice and (b) the user's stashed edits are OUTSIDE that
        # block. When the user edited INSIDE the block, reset+reapply would
        # silently clobber their in-block edit — so skip the reset and let the
        # plain pop's 3-way merge surface a genuine conflict (named below),
        # keeping the edit in the stash. When HEAD has no block at all, `init`'s
        # freshly appended block already survives an out-of-block pop unchanged,
        # so there is nothing to splice. Detect both by comparing the stashed
        # (user) block region against HEAD's block region.
        if [[ "$_reset_strategy" == "claude_md" ]]; then
          _head_block="$(git -C "$TARGET_PATH" show HEAD:CLAUDE.md 2>/dev/null | _emit_loom_claude_block)" || true
          [[ -n "$_head_block" ]] || continue
          _stashed_block="$(git -C "$TARGET_PATH" show 'stash@{0}:CLAUDE.md' 2>/dev/null | _emit_loom_claude_block)" || true
          [[ "$_stashed_block" == "$_head_block" ]] || continue
        fi

        _reset_snap="$(mktemp 2>/dev/null || true)"
        [[ -n "$_reset_snap" ]] || continue
        if cp "$TARGET_PATH/$_owned_path" "$_reset_snap" 2>/dev/null && \
           git -C "$TARGET_PATH" checkout HEAD -- "$_owned_path" 2>/dev/null; then
          REINSTALL_RESET_PATHS+=("$_owned_path")
          REINSTALL_RESET_SNAPSHOTS+=("$_reset_snap")
          REINSTALL_RESET_STRATEGIES+=("$_reset_strategy")
        else
          rm -f "$_reset_snap" 2>/dev/null || true
        fi
      done

      # Issue #3611: pop with `--index` so a caller's pre-existing staged/
      # unstaged split is reproduced. A plain `git stash pop` re-applies EVERY
      # stashed hunk to the working tree as *unstaged* — a caller who had a
      # `.gitignore` edit STAGED before the reinstall got it back unstaged, and
      # any careful partial staging in flight was silently flattened. `--index`
      # reinstates the index tree the stash recorded at push time, so staged
      # hunks come back staged and unstaged hunks stay unstaged. The `.gitignore`
      # HEAD-reset above provides a clean 3-way base so the index restore lines
      # up; `reapply_loom_gitignore_patterns` (below) then appends Loom ephemeral
      # patterns to the WORKING TREE ONLY (never the staged copy — they are not
      # the caller's change). `--index` is stricter than a plain pop: if it
      # cannot reinstate the index cleanly (a genuine conflict) it fails, and we
      # fall through to the conflict-surfacing branch below rather than silently
      # degrading to an unstaged pop that would drop the staged split.
      #
      # Capture the pop in an `if` condition so the assignment is exempt from
      # `set -e`. A plain top-level `VAR="$(cmd)"` assignment inherits the
      # command-substitution exit status, so a conflicting `git stash pop`
      # (non-zero) would trip `set -euo pipefail` on the assignment itself and
      # abort the installer before the conflict-surfacing branch below ever
      # runs (issue #3588 / PR review).
      if REINSTALL_POP_OUTPUT="$(git -C "$TARGET_PATH" stash pop --index 2>&1)"; then
        REINSTALL_POP_STATUS=0
      else
        REINSTALL_POP_STATUS=$?
      fi

      if [[ $REINSTALL_POP_STATUS -eq 0 ]]; then
        # Pop succeeded. Each file we reset to HEAD now carries the user's hunk
        # but its OLD committed Loom content — re-apply the fresh Loom portion
        # from that file's post-init snapshot (append for .gitignore, marker-
        # block splice for CLAUDE.md).
        _reset_i=0
        while [[ $_reset_i -lt ${#REINSTALL_RESET_PATHS[@]} ]]; do
          case "${REINSTALL_RESET_STRATEGIES[$_reset_i]}" in
            gitignore)
              reapply_loom_gitignore_patterns "$TARGET_PATH" "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}"
              ;;
            claude_md)
              reapply_loom_claude_md_block "$TARGET_PATH" "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}"
              ;;
          esac
          _reset_i=$((_reset_i + 1))
        done
        success "User changes restored"
      else
        # Genuine conflict (e.g. the user also edited the same lines a Loom-owned
        # file that `init` rewrote occupies). Roll every reset file back to its
        # post-init snapshot so the tree is not left half-reset, then surface the
        # real conflict — naming the specific file(s) that conflicted — and a
        # concrete recovery path. Do NOT abort — the reinstall itself succeeded;
        # only the user-change restore needs manual attention.
        _reset_i=0
        while [[ $_reset_i -lt ${#REINSTALL_RESET_PATHS[@]} ]]; do
          cp "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}" \
            "$TARGET_PATH/${REINSTALL_RESET_PATHS[$_reset_i]}" 2>/dev/null || true
          _reset_i=$((_reset_i + 1))
        done
        # Issue #3663: name the file(s) that actually conflicted rather than a
        # generic "recover by hand". Prefer git's own unmerged set; fall back to
        # the guarded Loom-owned dirty set when git reports none.
        REINSTALL_CONFLICT_FILES="$(git -C "$TARGET_PATH" diff --name-only --diff-filter=U 2>/dev/null | paste -sd' ' - 2>/dev/null || true)"
        if [[ -z "$REINSTALL_CONFLICT_FILES" ]]; then
          REINSTALL_CONFLICT_FILES="${REINSTALL_OWNED_DIRTY[*]-}"
        fi
        REINSTALL_STASH_REF="$(git -C "$TARGET_PATH" stash list 2>/dev/null | head -1 | cut -d: -f1)"
        [[ -z "$REINSTALL_STASH_REF" ]] && REINSTALL_STASH_REF="stash@{0}"
        warning "Failed to restore stashed user changes automatically"
        echo ""
        [[ -n "$REINSTALL_CONFLICT_FILES" ]] && \
          echo "  Conflicting file(s): $REINSTALL_CONFLICT_FILES" && echo ""
        echo "  git stash pop --index reported:"
        printf '%s\n' "$REINSTALL_POP_OUTPUT" | sed 's/^/    /'
        echo ""
        echo "  Note: the restore preserves your original staged/unstaged split"
        echo "  (git stash pop --index). That split could not be reproduced"
        echo "  automatically here, so recover by hand to keep it intact."
        echo "  Your changes are preserved in the stash ($REINSTALL_STASH_REF)."
        echo "  A plain 'git stash pop' will conflict the same way, so recover by hand:"
        echo "    cd $TARGET_PATH"
        echo "    git stash show -p $REINSTALL_STASH_REF              # inspect the stashed diff"
        echo "    git stash show -p $REINSTALL_STASH_REF | git apply --3way   # or reconcile by hand"
        echo "    git stash drop $REINSTALL_STASH_REF                 # once you've reconciled"
      fi

      _reset_i=0
      while [[ $_reset_i -lt ${#REINSTALL_RESET_SNAPSHOTS[@]} ]]; do
        rm -f "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}" 2>/dev/null || true
        _reset_i=$((_reset_i + 1))
      done
    fi

    echo ""
    success "Quick reinstallation complete!"
    exit 0
  fi

  # Default: delegate to Full Install (creates worktree + PR)
  info "Running fresh install via Full Install workflow..."
  echo ""
  INSTALL_FLAGS=()
  if [[ "$NON_INTERACTIVE" == true ]]; then
    INSTALL_FLAGS+=(--yes)
  fi
  exec "$LOOM_ROOT/scripts/install-loom.sh" ${INSTALL_FLAGS[@]+"${INSTALL_FLAGS[@]}"} ${SOURCE_OVERRIDE_FLAGS[@]+"${SOURCE_OVERRIDE_FLAGS[@]}"} "$TARGET_PATH"
else
  FORCE_FLAG=""
  SELF_INSTALL=false
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
echo ""
info "Tooling (committed to git):"
echo "  • .claude/commands/loom/*.md - Slash commands for Claude Code"
echo "  • .github/labels.yml        - Workflow label definitions"
echo "  • .github/ISSUE_TEMPLATE/   - Issue templates"
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
if [[ "$NON_INTERACTIVE" != true ]]; then
  read -r -p "Proceed with installation? [y/N] " -n 1 PROCEED
  echo ""
  if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
else
  info "Non-interactive mode: proceeding with installation"
fi

# Determine installation method
if [[ -n "$INSTALL_TYPE" ]]; then
  # Installation type was specified via --quick or --full flag
  METHOD="$INSTALL_TYPE"
  if [[ "$METHOD" == "1" ]]; then
    info "Using Quick Install (via --quick flag)"
  else
    info "Using Full Install (via --full flag)"
  fi
elif [[ "$NON_INTERACTIVE" == true ]]; then
  # Non-interactive mode without explicit type defaults to quick install
  METHOD="1"
  info "Non-interactive mode: defaulting to Quick Install"
else
  # Interactive mode: show options and prompt
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
fi

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

    # Export LOOM_VERSION / LOOM_COMMIT so the daemon's template substituter
    # fills CLAUDE.md correctly (issue #3502).
    prepare_loom_metadata_env "$LOOM_ROOT"

    # Handle --clean: run local uninstall first, then fresh install
    if [[ "$FORCE_FLAG" == "--clean" ]]; then
      info "Running local uninstall before fresh install..."
      "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
        error "Uninstall failed - aborting clean install"
      echo ""
      info "Uninstall complete, proceeding with fresh install..."
      "$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    else
      # Run loom-daemon init
      "$LOOM_ROOT/target/release/loom-daemon" init $FORCE_FLAG --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    fi

    # Install hooks and CLI wrapper (not handled by loom-daemon init).
    # Force-overwrite existing hooks only under --clean (a deliberate fresh
    # install); otherwise preserve a downstream-tuned hook (#3625).
    _HOOK_FORCE=false
    [[ "$FORCE_FLAG" == "--clean" ]] && _HOOK_FORCE=true
    install_hooks_and_cli "$LOOM_ROOT" "$TARGET_PATH" "$_HOOK_FORCE"
    # Emit skill-routes.json, install-metadata.json, loom-source-path (#3502).
    finalize_quick_install "$LOOM_ROOT" "$TARGET_PATH"
    verify_install "$TARGET_PATH"

    echo ""
    success "Quick installation complete!"
    ;;

  2)
    info "Running Full Install with Workflow..."
    echo ""

    # Detect forge type from remote URL
    cd "$TARGET_PATH"
    _ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
    _DETECTED_FORGE="github"
    if [[ -n "$_ORIGIN_URL" ]] && [[ ! "$_ORIGIN_URL" =~ github\.com ]]; then
      _DETECTED_FORGE="gitea"
    fi

    # Check prerequisites based on detected forge
    if [[ "$_DETECTED_FORGE" == "github" ]]; then
      if ! command -v gh &> /dev/null; then
        error "GitHub CLI (gh) is required for GitHub repos\n       Install: brew install gh\n       For Gitea repos, set GITEA_TOKEN instead"
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
    else
      # Gitea forge
      if [[ -z "${GITEA_TOKEN:-${FORGE_TOKEN:-}}" ]]; then
        warning "Gitea detected but no API token found"
        info "Set GITEA_TOKEN or FORGE_TOKEN environment variable"
        info "Create a token at: <your-gitea-instance>/user/settings/applications"
      else
        success "Gitea API token configured"
      fi
    fi
    echo ""

    # Show repository info
    REPO_NAME="unknown"
    if [[ "$_DETECTED_FORGE" == "github" ]]; then
      REPO_INFO=$(gh repo view --json nameWithOwner,description 2>/dev/null || echo "{}")
      REPO_NAME=$(echo "$REPO_INFO" | jq -r '.nameWithOwner // "unknown"' 2>/dev/null || echo "unknown")
    elif [[ -n "$_ORIGIN_URL" ]]; then
      REPO_NAME=$(echo "$_ORIGIN_URL" | sed -E 's/\.git$//; s#^.*[:/]([^/]+/[^/]+)$#\1#' || echo "unknown")
    fi

    if [[ "$REPO_NAME" != "unknown" ]]; then
      info "Target repository: $REPO_NAME (${_DETECTED_FORGE})"
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
    exec "$LOOM_ROOT/scripts/install-loom.sh" $FORCE_FLAG ${SOURCE_OVERRIDE_FLAGS[@]+"${SOURCE_OVERRIDE_FLAGS[@]}"} "$TARGET_PATH"
    ;;
esac

echo ""
header "═══════════════════════════════════════════════════════════"
echo ""
