#!/usr/bin/env bash
# Set up Python virtual environment and install loom-tools
#
# This script handles PEP 668 compliant systems by creating an isolated venv.
# It's designed to work on both:
# - macOS with Homebrew Python (externally managed)
# - Standard Python installations
#
# Usage:
#   ./scripts/install/setup-python-tools.sh [OPTIONS]
#
# Options:
#   --loom-root <path>  Path to Loom source repository (required)
#   --quiet             Suppress progress output
#   --force             Recreate venv even if it exists
#   --check             Just check if setup is complete, exit 0 if yes
#
# Exit codes:
#   0 - Success (or --check found valid installation)
#   1 - Installation failed
#   2 - Missing requirements

set -euo pipefail

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Default values
LOOM_ROOT=""
QUIET=false
FORCE=false
CHECK_ONLY=false

error() {
  echo -e "${RED}Error: $*${NC}" >&2
  exit 1
}

info() {
  [[ "$QUIET" == "true" ]] || echo -e "${BLUE}ℹ $*${NC}"
}

success() {
  [[ "$QUIET" == "true" ]] || echo -e "${GREEN}✓ $*${NC}"
}

warning() {
  echo -e "${YELLOW}⚠ $*${NC}"
}

# Parse arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    --loom-root)
      LOOM_ROOT="$2"
      shift 2
      ;;
    --quiet)
      QUIET=true
      shift
      ;;
    --force)
      FORCE=true
      shift
      ;;
    --check)
      CHECK_ONLY=true
      shift
      ;;
    -h|--help)
      echo "Usage: $0 --loom-root <path> [--quiet] [--force] [--check]"
      echo ""
      echo "Sets up Python virtual environment and installs loom-tools."
      echo ""
      echo "Options:"
      echo "  --loom-root <path>  Path to Loom source repository (required)"
      echo "  --quiet             Suppress progress output"
      echo "  --force             Recreate venv even if it exists"
      echo "  --check             Just check if setup is complete"
      exit 0
      ;;
    *)
      error "Unknown option: $1"
      ;;
  esac
done

# Validate arguments
if [[ -z "$LOOM_ROOT" ]]; then
  error "Missing required argument: --loom-root"
fi

if [[ ! -d "$LOOM_ROOT" ]]; then
  error "Loom root directory does not exist: $LOOM_ROOT"
fi

LOOM_TOOLS="$LOOM_ROOT/loom-tools"

if [[ ! -d "$LOOM_TOOLS" ]]; then
  error "loom-tools directory not found: $LOOM_TOOLS"
fi

if [[ ! -f "$LOOM_TOOLS/pyproject.toml" ]]; then
  error "pyproject.toml not found in loom-tools"
fi

VENV_PATH="$LOOM_TOOLS/.venv"
LOOM_SHEPHERD="$VENV_PATH/bin/loom-shepherd"

# Check-only mode
if [[ "$CHECK_ONLY" == "true" ]]; then
  if [[ -x "$LOOM_SHEPHERD" ]]; then
    success "loom-tools is installed and ready"
    exit 0
  else
    exit 1
  fi
fi

# Check if already installed (unless --force)
if [[ "$FORCE" != "true" ]] && [[ -x "$LOOM_SHEPHERD" ]]; then
  success "loom-tools already installed"
  exit 0
fi

# Find Python 3
PYTHON=""
for py in python3.12 python3.11 python3.10 python3; do
  if command -v "$py" &>/dev/null; then
    version=$("$py" --version 2>&1 | grep -oE '[0-9]+\.[0-9]+' | head -1)
    major=$(echo "$version" | cut -d. -f1)
    minor=$(echo "$version" | cut -d. -f2)
    if [[ "$major" -ge 3 ]] && [[ "$minor" -ge 10 ]]; then
      PYTHON="$py"
      break
    fi
  fi
done

if [[ -z "$PYTHON" ]]; then
  error "Python 3.10+ is required but not found"
fi

info "Using Python: $PYTHON ($("$PYTHON" --version 2>&1))"

# Remove existing venv if --force
if [[ "$FORCE" == "true" ]] && [[ -d "$VENV_PATH" ]]; then
  info "Removing existing virtual environment..."
  rm -rf "$VENV_PATH"
fi

# Create virtual environment
if [[ ! -d "$VENV_PATH" ]]; then
  info "Creating virtual environment..."
  "$PYTHON" -m venv "$VENV_PATH" || error "Failed to create virtual environment"
fi

# Upgrade pip in venv
info "Upgrading pip..."
"$VENV_PATH/bin/pip" install --upgrade pip --quiet || warning "pip upgrade failed (non-fatal)"

# Install loom-tools in editable mode
info "Installing loom-tools..."
"$VENV_PATH/bin/pip" install -e "$LOOM_TOOLS" --quiet || error "Failed to install loom-tools"

# Verify installation
if [[ ! -x "$LOOM_SHEPHERD" ]]; then
  error "Installation verification failed: loom-shepherd not found"
fi

# Test that it runs
if ! "$LOOM_SHEPHERD" --help &>/dev/null; then
  error "Installation verification failed: loom-shepherd cannot run"
fi

success "loom-tools installed successfully"
info "Installed commands:"
for cmd in "$VENV_PATH/bin/loom-"*; do
  if [[ -x "$cmd" ]]; then
    echo "  - $(basename "$cmd")"
  fi
done
