#!/usr/bin/env bash
# Generate .claude/agents/ files from .loom/roles/ source files
# This script ensures .loom/roles/ is the single source of truth for role definitions
# while .claude/agents/ files are generated with YAML frontmatter for Claude Code

set -euo pipefail

# Determine script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Source and destination directories
ROLES_DIR="$REPO_ROOT/defaults/roles"
AGENTS_DIR="$REPO_ROOT/defaults/.claude/agents"

# ANSI color codes
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

info() {
  echo -e "${BLUE}ℹ $*${NC}"
}

success() {
  echo -e "${GREEN}✓ $*${NC}"
}

warning() {
  echo -e "${YELLOW}⚠ $*${NC}"
}

# Validate directories exist
if [[ ! -d "$ROLES_DIR" ]]; then
  echo "Error: Roles directory not found: $ROLES_DIR" >&2
  exit 1
fi

if [[ ! -d "$AGENTS_DIR" ]]; then
  mkdir -p "$AGENTS_DIR"
  info "Created agents directory: $AGENTS_DIR"
fi

info "Generating Claude Code agents from role definitions..."
echo ""
info "Source: $ROLES_DIR"
info "Destination: $AGENTS_DIR"
echo ""

# Counter for generated files
count=0

# Process each .md file in roles directory
for role_file in "$ROLES_DIR"/*.md; do
  # Skip if no .md files found
  [[ -e "$role_file" ]] || continue

  # Extract filename without path
  filename=$(basename "$role_file")
  role_name="${filename%.md}"

  # Skip README.md
  if [[ "$role_name" == "README" ]]; then
    info "Skipping README.md"
    continue
  fi

  # Check if corresponding .json metadata file exists
  json_file="$ROLES_DIR/${role_name}.json"

  # Extract metadata from JSON if it exists
  if [[ -f "$json_file" ]]; then
    # Read JSON and extract fields using node
    if command -v node &> /dev/null; then
      description=$(node -pe "try { require('$json_file').description || 'Agent role' } catch(e) { 'Agent role' }")

      # Extract tools from JSON or use defaults
      tools=$(node -pe "try { JSON.parse(require('fs').readFileSync('$json_file', 'utf8')).tools || null } catch(e) { null }")
      if [[ "$tools" == "null" ]]; then
        # Default tools based on role name
        case "$role_name" in
          builder|healer|architect|driver)
            tools="Bash, Read, Write, Edit, Grep, Glob, TodoWrite, Task"
            ;;
          *)
            tools="Bash, Read, Grep, Glob, Task"
            ;;
        esac
      fi

      # Extract model from JSON or use default (sonnet)
      model=$(node -pe "try { require('$json_file').model || 'sonnet' } catch(e) { 'sonnet' }")
    else
      warning "Node.js not found, using default metadata for $role_name"
      description="Agent role"
      tools="Bash, Read, Grep, Glob, Task"
      model="sonnet"
    fi
  else
    warning "No metadata file found for $role_name, using defaults"
    description="Agent role"

    # Default tools based on role name
    case "$role_name" in
      builder|healer|architect|driver)
        tools="Bash, Read, Write, Edit, Grep, Glob, TodoWrite, Task"
        ;;
      *)
        tools="Bash, Read, Grep, Glob, Task"
        ;;
    esac

    model="sonnet"
  fi

  # Output file path
  agent_file="$AGENTS_DIR/$filename"

  info "Generating $filename..."

  # Generate agent file with YAML frontmatter
  {
    echo "---"
    echo "name: $role_name"
    echo "description: $description"
    echo "tools: $tools"
    echo "model: $model"
    echo "---"
    echo ""
    cat "$role_file"
  } > "$agent_file"

  success "Generated $filename"
  ((count++))
done

echo ""
success "Generated $count Claude Code agent files"
echo ""
info "Next steps:"
info "  1. Review generated files in $AGENTS_DIR"
info "  2. Commit changes if they look correct"
info "  3. Run this script whenever .loom/roles/ files are updated"
