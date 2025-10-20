# Loom CLI Reference

Complete reference for all Loom command-line interface commands and options.

## Table of Contents

- [Overview](#overview)
- [`loom-daemon init`](#loom-daemon-init)
  - [Synopsis](#synopsis)
  - [Description](#description)
  - [Options](#options)
  - [Examples](#examples)
  - [Exit Codes](#exit-codes)
  - [Environment Variables](#environment-variables)
- [Common Workflows](#common-workflows)
- [Troubleshooting](#troubleshooting)

## Overview

The Loom daemon provides a command-line interface for headless operations, primarily focused on workspace initialization without requiring the GUI application.

**Binary Name:** `loom-daemon`

**Primary Use Cases:**
- Initializing Loom workspaces in CI/CD pipelines
- Headless server setup
- Scripted bulk initialization across multiple repositories
- Manual orchestration without GUI

## `loom-daemon init`

Initialize a Loom workspace in a git repository.

### Synopsis

```bash
loom-daemon init [OPTIONS] [PATH]
```

### Description

The `init` subcommand sets up a Loom workspace by:

1. **Validating** the target directory is a git repository
2. **Copying** `.loom/` configuration from defaults
3. **Installing** repository scaffolding:
   - `CLAUDE.md` - AI context documentation
   - `AGENTS.md` - Agent workflow guide
   - `.claude/` - Claude Code configuration
   - `.codex/` - Codex configuration (if available)
   - `.github/` - GitHub workflow templates and labels
4. **Updating** `.gitignore` with Loom ephemeral patterns

The initialization process is **idempotent** - it only creates files that don't already exist (unless `--force` is used).

### Options

#### Positional Arguments

##### `PATH`

Target directory to initialize (must be a git repository).

- **Type:** String (absolute or relative path)
- **Default:** Current working directory (`.`)
- **Examples:**
  ```bash
  loom-daemon init                           # Current directory
  loom-daemon init /path/to/repo             # Absolute path
  loom-daemon init ../my-project             # Relative path
  loom-daemon init ~/Projects/my-repo        # Home directory expansion
  ```

#### Flags

##### `--force`

Overwrite existing `.loom` directory if it exists.

- **Type:** Boolean flag (no value)
- **Default:** `false`
- **Behavior:**
  - If `.loom/` exists and `--force` is NOT set: exit with error
  - If `.loom/` exists and `--force` IS set: remove and recreate
- **Use Cases:**
  - Resetting workspace to factory defaults
  - Recovering from corrupted configuration
  - Upgrading Loom configuration to newer version
- **Example:**
  ```bash
  loom-daemon init --force
  ```

**⚠️ Warning:** This will DELETE the entire `.loom/` directory, including:
- Custom role definitions in `.loom/roles/`
- Terminal configurations in `.loom/config.json`
- Any other customizations

##### `--dry-run`

Preview what would be changed without making any modifications.

- **Type:** Boolean flag (no value)
- **Default:** `false`
- **Behavior:**
  - Validates the target is a git repository
  - Shows what files would be created/updated
  - Does NOT create or modify any files
  - Exit code indicates whether init would succeed
- **Use Cases:**
  - Previewing changes before applying
  - Validating repository before initialization
  - CI/CD pipeline dry runs
- **Example:**
  ```bash
  loom-daemon init --dry-run
  ```
- **Output:**
  ```
  [DRY RUN] Would initialize workspace: /path/to/repo
  [DRY RUN] Would create: .loom/config.json
  [DRY RUN] Would create: .loom/roles/
  [DRY RUN] Would create: CLAUDE.md
  [DRY RUN] Would update: .gitignore
  [DRY RUN] Workspace validation: ✓ Valid git repository
  ```

##### `--defaults <PATH>`

Specify custom defaults directory instead of bundled defaults.

- **Type:** String (path to defaults directory)
- **Default:** Bundled `defaults/` directory
- **Resolution Order:**
  1. Provided path (relative to current directory)
  2. Git repository root + path (handles worktrees)
  3. Bundled resource path (production builds)
- **Use Cases:**
  - Custom organizational defaults
  - Team-specific role definitions
  - Testing new default configurations
  - Development/debugging
- **Example:**
  ```bash
  loom-daemon init --defaults ./custom-defaults
  loom-daemon init --defaults /path/to/org/loom-defaults
  ```

**Defaults Directory Structure:**

The defaults directory must contain:
```
defaults/
├── config.json           # Default config template
├── CLAUDE.md             # AI context template
├── AGENTS.md             # Agent workflow template
├── .loom-README.md       # .loom/ directory documentation
├── roles/                # Role definitions
│   ├── architect.md
│   ├── builder.md
│   ├── curator.md
│   ├── driver.md
│   ├── guide.md
│   ├── healer.md
│   ├── hermit.md
│   └── judge.md
├── .claude/              # Claude Code config
├── .codex/               # Codex config (optional)
└── .github/              # GitHub templates
    ├── labels.yml
    └── workflows/
```

#### Flag Interactions

Flags can be combined:

```bash
# Preview force initialization with custom defaults
loom-daemon init --dry-run --force --defaults ./my-defaults

# Force init current directory
loom-daemon init --force

# Dry run on specific path
loom-daemon init --dry-run /path/to/repo
```

**Order doesn't matter:**
```bash
loom-daemon init --force --dry-run /path/to/repo
# Same as:
loom-daemon init /path/to/repo --dry-run --force
```

### Examples

#### Basic Usage

```bash
# Initialize current directory
cd /path/to/your/repo
loom-daemon init

# Initialize specific directory
loom-daemon init /path/to/another/repo

# Initialize with home directory expansion
loom-daemon init ~/Projects/my-repo
```

#### Preview Changes

```bash
# See what would be created without applying changes
loom-daemon init --dry-run

# Preview force initialization
loom-daemon init --dry-run --force
```

#### Force Reinitialization

```bash
# Reset workspace to defaults (deletes .loom/)
loom-daemon init --force

# Force with custom defaults
loom-daemon init --force --defaults ./custom-defaults
```

#### Custom Defaults

```bash
# Use organizational defaults
loom-daemon init --defaults /path/to/org/loom-config

# Use defaults from different location
loom-daemon init --defaults ~/loom-defaults /path/to/repo
```

#### CI/CD Integration

```bash
# GitHub Actions - initialize workspace
- name: Initialize Loom
  run: loom-daemon init --force

# GitLab CI - with custom defaults
loom_setup:
  script:
    - loom-daemon init --defaults $CI_PROJECT_DIR/defaults

# Jenkins - dry run first
sh 'loom-daemon init --dry-run || exit 1'
sh 'loom-daemon init'
```

### Exit Codes

| Code | Meaning | Common Causes |
|------|---------|---------------|
| `0` | Success | Workspace initialized successfully |
| `1` | General error | Multiple possible causes (see stderr) |

**Error Scenarios** (exit code `1`):

1. **Not a Git Repository**
   ```
   Error: Not a git repository (no .git directory found): /path
   ```
   - Path doesn't contain `.git` directory
   - Solution: Run `git init` first or use correct path

2. **Already Initialized**
   ```
   Error: Workspace already initialized (.loom directory exists). Use --force to overwrite.
   ```
   - `.loom/` directory already exists
   - Solution: Use `--force` flag or remove `.loom/` manually

3. **Permission Denied**
   ```
   Error: Failed to create .loom directory: Permission denied
   ```
   - Insufficient permissions to write to directory
   - Solution: Check directory ownership and permissions

4. **Defaults Not Found**
   ```
   Error: Defaults directory not found. Tried paths:
     ./defaults
     /path/to/git/root/defaults
     /Applications/Loom.app/Contents/Resources/_up_/defaults
   ```
   - Defaults directory couldn't be resolved
   - Solution: Specify `--defaults` explicitly or check installation

5. **Path Does Not Exist**
   ```
   Error: Path does not exist: /nonexistent/path
   ```
   - Target path doesn't exist
   - Solution: Create directory first or use correct path

6. **Not a Directory**
   ```
   Error: Path is not a directory: /path/to/file
   ```
   - Target path is a file, not a directory
   - Solution: Use correct directory path

### Environment Variables

#### `LOOM_DEFAULTS_PATH`

Override default location for defaults directory.

- **Type:** String (absolute path)
- **Precedence:** Command-line `--defaults` flag takes priority
- **Example:**
  ```bash
  export LOOM_DEFAULTS_PATH=/usr/local/share/loom/defaults
  loom-daemon init  # Uses LOOM_DEFAULTS_PATH

  # Override with flag
  loom-daemon init --defaults ./custom  # Uses ./custom, not LOOM_DEFAULTS_PATH
  ```

#### `LOOM_SOCKET_PATH`

Specify custom Unix socket path for daemon IPC.

- **Type:** String (absolute path to socket file)
- **Default:** `~/.loom/loom-daemon.sock`
- **Use Cases:**
  - Running multiple daemon instances
  - Testing and development
  - Avoiding conflicts
- **Example:**
  ```bash
  export LOOM_SOCKET_PATH=/tmp/loom-test.sock
  loom-daemon start
  ```

#### `RUST_LOG`

Control logging verbosity (standard Rust logging).

- **Type:** String (log level)
- **Levels:** `error`, `warn`, `info`, `debug`, `trace`
- **Example:**
  ```bash
  # Enable debug logging
  RUST_LOG=debug loom-daemon init

  # Trace everything
  RUST_LOG=trace loom-daemon init

  # Only errors
  RUST_LOG=error loom-daemon init
  ```

## Common Workflows

### Workflow 1: First-Time Setup

```bash
# 1. Clone repository
git clone https://github.com/org/repo.git
cd repo

# 2. Preview initialization
loom-daemon init --dry-run

# 3. Initialize
loom-daemon init

# 4. Verify
ls -la .loom
cat .loom/config.json
```

### Workflow 2: Reset to Defaults

```bash
# 1. Backup current config (optional)
cp .loom/config.json .loom/config.json.bak

# 2. Force reinitialization
loom-daemon init --force

# 3. Verify reset
cat .loom/config.json
```

### Workflow 3: Organization-Wide Defaults

```bash
# 1. Create shared defaults repository
git clone https://github.com/org/loom-defaults.git ~/loom-defaults

# 2. Initialize projects with org defaults
loom-daemon init --defaults ~/loom-defaults /path/to/project1
loom-daemon init --defaults ~/loom-defaults /path/to/project2

# 3. Update defaults and reinitialize
cd ~/loom-defaults
git pull
loom-daemon init --force --defaults ~/loom-defaults /path/to/project1
```

### Workflow 4: CI/CD Pipeline

```bash
# .github/workflows/ci.yml
- name: Setup Loom
  run: |
    # Download loom-daemon binary
    curl -L https://github.com/org/loom/releases/download/v0.1.0/loom-daemon -o loom-daemon
    chmod +x loom-daemon

    # Initialize workspace
    ./loom-daemon init --force

    # Verify
    ls -la .loom
```

### Workflow 5: Bulk Initialization

```bash
# Initialize multiple repositories
for repo in ~/Projects/*/; do
  echo "Initializing $repo"
  loom-daemon init --force "$repo"
done

# Or with find
find ~/Projects -name ".git" -type d -execdir sh -c 'loom-daemon init --force "$PWD"' \;
```

## Troubleshooting

### Issue: "Command not found: loom-daemon"

**Cause:** Binary not in PATH

**Solutions:**
```bash
# Option 1: Add to PATH
export PATH="/path/to/loom/target/release:$PATH"

# Option 2: Use absolute path
/path/to/loom/target/release/loom-daemon init

# Option 3: Create symlink
ln -s /path/to/loom/target/release/loom-daemon /usr/local/bin/loom-daemon
```

### Issue: "Defaults directory not found"

**Cause:** Cannot locate defaults directory

**Solutions:**
```bash
# Option 1: Specify explicitly
loom-daemon init --defaults /path/to/loom/defaults

# Option 2: Set environment variable
export LOOM_DEFAULTS_PATH=/path/to/loom/defaults
loom-daemon init

# Option 3: Check bundled path (production)
ls /Applications/Loom.app/Contents/Resources/_up_/defaults
```

### Issue: ".loom already exists" but appears empty

**Cause:** Partially created directory from failed initialization

**Solutions:**
```bash
# Option 1: Force reinitialization
loom-daemon init --force

# Option 2: Manual cleanup
rm -rf .loom
loom-daemon init

# Option 3: Inspect and repair
ls -la .loom
# If missing files, use --force to complete
```

### Issue: Permission errors

**Cause:** Insufficient permissions to write to directory

**Solutions:**
```bash
# Option 1: Fix ownership
sudo chown -R $(whoami) /path/to/repo

# Option 2: Check parent directory permissions
ls -la /path/to/
chmod u+w /path/to/repo

# Option 3: Run from user-owned directory
cd ~/Projects
git clone repo
cd repo
loom-daemon init
```

### Issue: Silent failure (no error, no .loom directory)

**Cause:** Check exit code and stderr

**Debugging:**
```bash
# Check exit code
loom-daemon init
echo $?  # Should be 0 for success

# Enable debug logging
RUST_LOG=debug loom-daemon init

# Use dry run to test
loom-daemon init --dry-run
```

### Issue: Corrupted defaults

**Cause:** Invalid files in defaults directory

**Solutions:**
```bash
# Option 1: Re-clone Loom repository
git clone https://github.com/rjwalters/loom.git
cd loom
loom-daemon init --defaults ./defaults /path/to/target

# Option 2: Download fresh defaults
curl -L https://github.com/rjwalters/loom/archive/main.zip -o loom.zip
unzip loom.zip
loom-daemon init --defaults ./loom-main/defaults /path/to/target
```

## See Also

- [Getting Started](getting-started.md) - Installation and setup guide
- [CI/CD Setup](ci-cd-setup.md) - Pipeline integration examples
- [Common Tasks](common-tasks.md) - Development workflows
- [Troubleshooting](troubleshooting.md) - Debugging guides

## Quick Reference

```bash
# Most common commands
loom-daemon init                           # Initialize current directory
loom-daemon init /path/to/repo             # Initialize specific directory
loom-daemon init --force                   # Reset to defaults
loom-daemon init --dry-run                 # Preview changes
loom-daemon init --defaults ./custom       # Custom defaults

# Flags
--force          # Overwrite existing .loom directory
--dry-run        # Preview without changes
--defaults PATH  # Custom defaults location

# Exit codes
0  # Success
1  # Error (check stderr)
```
