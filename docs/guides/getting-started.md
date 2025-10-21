# Getting Started with Loom

This comprehensive guide walks you through installing and setting up Loom, whether you're an end user wanting to use Loom in your repository or a contributor building Loom itself.

## Table of Contents

- [Before You Install](#before-you-install)
- [Prerequisites](#prerequisites)
- [Installation Options](#installation-options)
  - [Option 1: Download Binary (Easiest)](#option-1-download-binary-easiest)
  - [Option 2: Build from Source](#option-2-build-from-source)
  - [Option 3: Interactive Install Script](#option-3-interactive-install-script)
  - [Option 4: GUI Application](#option-4-gui-application)
- [First-Time Setup](#first-time-setup)
- [Verifying Your Setup](#verifying-your-setup)
- [Next Steps](#next-steps)
- [Troubleshooting](#troubleshooting)

## Before You Install

### What Loom Does

Loom transforms your repository into an AI-orchestrated workspace where agents coordinate through GitHub issues, PRs, and labels. Each terminal can embody a specialized role (Worker, Curator, Architect, Reviewer) working autonomously or on-demand.

### What Gets Installed

Running `loom-daemon init` creates these files in your repository:

**Configuration (Commit these)**:
- `.loom/config.json` - Terminal settings and role assignments
- `.loom/roles/` - Custom agent role definitions (optional)

**Documentation (Commit these)**:
- `CLAUDE.md` - AI context document for Claude Code (11KB template)
- `AGENTS.md` - Workflow coordination guide for agents

**Tooling (Commit these)**:
- `.claude/commands/` - Claude Code slash commands for each role
- `.codex/` - Codex configuration (if available)
- `.github/labels.yml` - Workflow label definitions

**Gitignored (Local only)**:
- `.loom/state.json` - Runtime terminal state
- `.loom/worktrees/` - Git worktrees for isolated work
- `.loom/*.log` - Application log files

### What Gets Modified

- **`.gitignore`** - Adds patterns for `.loom/state.json`, `.loom/worktrees/`, `~/.loom/console.log`, etc.

That's it! Loom is non-invasive and everything important can be committed to version control so your team shares the same agent configuration.

## Prerequisites

### For End Users (Using Loom)

Minimal requirements to use Loom:

1. **macOS** (currently macOS-only, Linux support planned)
2. **Git repository** (any existing project)
3. **tmux** (usually pre-installed on macOS)
   ```bash
   # Verify tmux is installed
   tmux -V

   # Install if needed (macOS)
   brew install tmux
   ```
4. **Claude Code** (optional, for AI agents)
   ```bash
   # Verify Claude Code is installed
   claude --version

   # See https://claude.com/claude-code for installation
   ```

That's all you need to use Loom!

### For Contributors (Developing Loom)

Additional requirements to build and contribute to Loom:

1. **Rust** (for Tauri backend compilation)
   ```bash
   # Install Rust via rustup (recommended)
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

   # Verify installation
   rustc --version
   cargo --version
   ```

2. **System Dependencies** (for Tauri)

   **macOS:**
   ```bash
   xcode-select --install
   ```

   **Linux (Ubuntu/Debian):**
   ```bash
   sudo apt update
   sudo apt install libwebkit2gtk-4.0-dev \
     build-essential \
     curl \
     wget \
     file \
     libssl-dev \
     libgtk-3-dev \
     libayatana-appindicator3-dev \
     librsvg2-dev
   ```

3. **Node.js** (v18 or later)
   ```bash
   # Install via nvm (recommended)
   nvm install 18

   # Verify installation
   node --version  # Should be v18+
   ```

4. **pnpm** (package manager)
   ```bash
   npm install -g pnpm

   # Verify installation
   pnpm --version
   ```

5. **GitHub CLI** (optional, for agent workflows)
   ```bash
   # macOS
   brew install gh

   # Linux - see https://cli.github.com/

   # Authenticate
   gh auth login
   ```

To verify all prerequisites:

```bash
# Check Rust (contributors only)
rustc --version && cargo --version

# Check Node.js (contributors only)
node --version

# Check pnpm (contributors only)
pnpm --version

# Check tmux (all users)
tmux -V

# Check Claude Code (optional)
claude --version

# Check GitHub CLI (optional)
gh --version
```

## Installation Options

Choose the installation method that best fits your needs:

### Option 1: Download Binary (Easiest)

Perfect for end users who want to use Loom without building from source.

```bash
# Download latest release
curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
chmod +x loom-daemon

# Initialize your repository
./loom-daemon init /path/to/your/repo
```

**What this does:**
- Downloads the pre-built daemon binary
- Makes it executable
- Initializes your repository with Loom configuration

**Next:** See [First-Time Setup](#first-time-setup) to explore what was created.

### Option 2: Build from Source

For contributors or users who want the latest development version.

```bash
# Clone Loom repository
git clone https://github.com/rjwalters/loom
cd loom

# Install dependencies
pnpm install

# Build daemon
pnpm daemon:build

# Initialize your repository
./target/release/loom-daemon init /path/to/your/repo
```

**What this does:**
- Clones the Loom source code
- Installs Node.js dependencies via pnpm
- Builds the Rust daemon from source
- Initializes your repository

**Next:** See [DEVELOPMENT.md](../../DEVELOPMENT.md) for development workflow.

### Option 3: Interactive Setup Script (Recommended)

Uses the interactive setup script for guided installation with two workflows.

```bash
# Clone Loom first (if you haven't)
git clone https://github.com/rjwalters/loom
cd loom

# Run interactive setup (will prompt for target repo if not provided)
./setup.sh /path/to/your/repo
```

**What this provides:**
- Interactive prompts for repository path
- Shows exactly what will be installed before proceeding
- Two installation methods:
  - **Quick Install**: Direct installation with `loom-daemon init`
  - **Full Install**: Creates GitHub issue, worktree, and PR for review
- Git repository validation
- GitHub authentication checks (for Full Install)
- Confirmation prompts at each step
- Clear error messages and recovery suggestions

**When to use Quick Install:**
- Personal projects or quick testing
- Solo development
- No need for GitHub issue tracking

**When to use Full Install:**
- Team projects requiring review
- Want installation tracked in GitHub issue
- Prefer git worktree isolation
- Need labels synced to repository

**Advanced:** For programmatic installation without prompts, use:
```bash
./scripts/install-loom.sh /path/to/your/repo
```

**Next:** Review the output to understand what was created.

### Option 4: GUI Application

Visual application with workspace management and terminal controls.

#### Steps

1. **Download and Install**
   - Download `Loom.app` from the [releases page](https://github.com/rjwalters/loom/releases)
   - Move `Loom.app` to your Applications folder
   - Open `Loom.app`

2. **Select Workspace**
   - Click "Choose Workspace" in the workspace selector
   - Navigate to your git repository
   - Select the repository root directory
   - Loom will validate that it's a valid git repository

3. **Initialize Workspace** (Automatic)
   - If this is the first time using Loom in this repository
   - The app will automatically create `.loom/` configuration
   - Default terminal roles will be installed

4. **Create Terminals**
   - Click "+" to create agent terminals
   - Configure each terminal's role via the settings icon
   - Assign specialized roles (Worker, Curator, Reviewer, etc.)

5. **Start Working**
   - Terminals are now ready for manual commands or AI agents
   - See [WORKFLOWS.md](../../WORKFLOWS.md) for agent coordination patterns

**Next:** Configure terminal roles via the settings UI.

### Initialization Flags

The `loom-daemon init` command supports several flags for customization:

```bash
# Initialize current directory
loom-daemon init

# Initialize specific repository
loom-daemon init /path/to/your/repo

# Preview changes without applying them
loom-daemon init --dry-run

# Overwrite existing .loom directory
loom-daemon init --force

# Use custom defaults directory
loom-daemon init --defaults ./custom-defaults
```

## First-Time Setup

After installing Loom (via GUI or CLI), you'll find the following files in your repository:

### Workspace Configuration (`.loom/`)

```
.loom/
├── config.json       # Terminal configurations, roles, agent counter
├── roles/            # Custom role definitions (initially empty)
└── README.md         # Documentation about .loom directory
```

**What to do:**
1. Review `config.json` to understand default terminal setup
2. Leave `roles/` empty unless you want custom role definitions
3. Read `.loom/README.md` for configuration guidance

### AI Context Documentation

```
CLAUDE.md             # Technical context for Claude Code agents
AGENTS.md             # Agent workflow and coordination guide
```

**What to do:**
1. Review `CLAUDE.md` to understand the codebase structure and patterns
2. Read `AGENTS.md` to learn about agent roles and workflows
3. Update `CLAUDE.md` with project-specific context as you build

### Claude Code Configuration

```
.claude/
├── commands/         # Slash commands for Claude Code
└── README.md         # Documentation
```

**What to do:**
1. Explore available slash commands in `.claude/commands/`
2. Add custom slash commands for your project
3. See [Claude Code docs](https://docs.claude.com/en/docs/claude-code) for details

### GitHub Configuration

```
.github/
├── labels.yml        # Label definitions for workflow coordination
└── workflows/        # CI/CD workflow templates
```

**What to do:**
1. Review label definitions in `labels.yml`
2. Sync labels to GitHub: `gh label sync -f .github/labels.yml`
3. Customize labels for your project's workflow

### Gitignore Updates

Loom automatically updates `.gitignore` with ephemeral patterns:

```gitignore
# Loom - AI Development Orchestration
.loom/state.json
.loom/worktrees/
.loom/*.log
.loom/*.sock
```

**What to commit:**
- ✅ `.loom/config.json` - Share terminal roles across team
- ✅ `.loom/roles/` - Custom role definitions
- ✅ `CLAUDE.md` - AI context documentation
- ✅ `AGENTS.md` - Agent workflow guide
- ✅ `.claude/` - Slash commands and config
- ✅ `.github/` - Labels and workflows

**What to gitignore:**
- ❌ `.loom/state.json` - Runtime state (session IDs, ephemeral data)
- ❌ `.loom/worktrees/` - Git worktrees (temporary workspaces)
- ❌ `.loom/*.log` - Log files
- ❌ `.loom/*.sock` - Unix socket files

## Verifying Your Setup

After installation, verify everything is working correctly:

### 1. Check File Structure

```bash
# Verify .loom directory structure
tree .loom

# Expected output:
# .loom
# ├── README.md
# ├── config.json
# └── roles
#     ├── architect.md
#     ├── builder.md
#     ├── curator.md
#     ├── driver.md
#     ├── guide.md
#     ├── healer.md
#     ├── hermit.md
#     └── judge.md
```

### 2. Check Configuration

```bash
# View config file (should have default terminals)
cat .loom/config.json

# Expected: JSON with nextAgentNumber and terminals array
```

### 3. Verify Gitignore

```bash
# Check gitignore was updated
grep -A 4 "Loom - AI Development Orchestration" .gitignore

# Expected:
# # Loom - AI Development Orchestration
# .loom/state.json
# .loom/worktrees/
# .loom/*.log
# .loom/*.sock
```

### 4. Test Daemon (Optional)

```bash
# Start daemon manually
loom-daemon start

# Check health
loom-daemon health

# Stop daemon
loom-daemon stop
```

### 5. Launch GUI (If Installed)

```bash
# Launch with current directory as workspace
open -a Loom --args --workspace $(pwd)

# Or launch and select workspace via UI
open -a Loom
```

## Next Steps

Now that Loom is installed and configured, you can:

### 1. Create Agent Terminals

**Via GUI:**
1. Click "+" button to add terminals
2. Click settings icon on each terminal
3. Assign roles (Builder, Judge, Curator, etc.)
4. Configure autonomous intervals if desired

**Via CLI:**
- Manually edit `.loom/config.json` to add terminals
- Define role assignments and autonomous settings
- Restart Loom to load new configuration

### 2. Set Up GitHub Labels

```bash
# Sync Loom workflow labels to GitHub
gh label sync -f .github/labels.yml

# Verify labels were created
gh label list | grep "loom:"
```

Labels enable workflow coordination between agents. See [WORKFLOWS.md](../../WORKFLOWS.md) for details.

### 3. Start Using Agents

#### Manual Mode (Builder, Healer, Driver)

```bash
# Launch Claude Code with a role
claude --role builder

# Follow the Builder workflow
# 1. Find "loom:ready" issue
# 2. Claim issue (add "loom:in-progress" label)
# 3. Create worktree: pnpm worktree <issue-number>
# 4. Implement, test, commit
# 5. Create PR with "loom:review-requested" label
```

#### Autonomous Mode (Judge, Curator, Architect, Hermit, Guide)

These roles run automatically at configured intervals:

- **Judge** (5 min) - Reviews PRs with `loom:review-requested`
- **Curator** (5 min) - Enhances issues, marks as `loom:ready`
- **Architect** (15 min) - Creates `loom:architect` proposals
- **Hermit** (15 min) - Identifies bloat, creates `loom:hermit` issues
- **Guide** (15 min) - Prioritizes issues with `loom:priority-*` labels

Configure intervals via terminal settings in the GUI.

### 4. Customize Roles

Create custom role definitions for your project:

```bash
# Create custom role
cat > .loom/roles/my-role.md <<'EOF'
# My Custom Role

You are a specialist in the {{workspace}} repository.

## Your Role

[Define the role's purpose and responsibilities]

## Your Workflow

[Define the workflow steps]
EOF

# Create metadata (optional)
cat > .loom/roles/my-role.json <<'EOF'
{
  "name": "My Custom Role",
  "description": "Brief description",
  "defaultInterval": 600000,
  "defaultIntervalPrompt": "Continue working",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
EOF
```

See [defaults/roles/README.md](../../defaults/roles/README.md) for role creation guidance.

### 5. Learn the Workflows

Read the comprehensive workflow documentation:

- [WORKFLOWS.md](../../WORKFLOWS.md) - Agent coordination patterns
- [Agent Archetypes](../philosophy/agent-archetypes.md) - Role philosophy
- [Git Workflow](git-workflow.md) - Branch strategy and PR process

## Troubleshooting

### Issue: "Not a git repository" Error

**Symptom:**
```
Error: Not a git repository (no .git directory found): /path/to/dir
```

**Solution:**
```bash
# Initialize git repository first
git init

# Or navigate to an existing git repository
cd /path/to/your/git/repo
loom-daemon init
```

### Issue: ".loom directory already exists"

**Symptom:**
```
Error: Workspace already initialized (.loom directory exists). Use --force to overwrite.
```

**Solution:**

**Option 1: Keep existing configuration**
```bash
# If .loom is already set up, you're done!
# No need to re-initialize
```

**Option 2: Reset to defaults**
```bash
# Overwrite with fresh defaults
loom-daemon init --force

# Or manually remove and re-initialize
rm -rf .loom
loom-daemon init
```

### Issue: "Permission denied" Errors

**Symptom:**
```
Error: Failed to create .loom directory: Permission denied
```

**Solution:**
```bash
# Check directory permissions
ls -la

# Ensure you own the directory
sudo chown -R $(whoami) /path/to/repo

# Or run with appropriate permissions
cd /path/to/repo  # as the owner
loom-daemon init
```

### Issue: "Defaults directory not found"

**Symptom:**
```
Error: Defaults directory not found. Tried paths: ...
```

**Solution:**

**For CLI users:**
```bash
# Specify defaults directory explicitly
loom-daemon init --defaults /path/to/loom/defaults

# Or use bundled defaults (production build)
loom-daemon init --defaults /Applications/Loom.app/Contents/Resources/_up_/defaults
```

**For developers:**
```bash
# Ensure you're in the Loom repository root
cd /path/to/loom
loom-daemon init /path/to/target/repo
```

### Issue: Corrupted Scaffolding Files

**Symptom:**
- `.loom/config.json` is invalid JSON
- Role files are empty or corrupted
- `CLAUDE.md` is malformed

**Solution:**
```bash
# Reset to factory defaults
loom-daemon init --force

# Or manually repair specific files
cp defaults/config.json .loom/config.json
cp defaults/CLAUDE.md ./CLAUDE.md
```

### Issue: Labels Not Syncing to GitHub

**Symptom:**
```
Error: gh: command not found
```

**Solution:**
```bash
# Install GitHub CLI
brew install gh

# Authenticate
gh auth login

# Sync labels
gh label sync -f .github/labels.yml
```

### Need More Help?

- **Documentation**: Check [docs/guides/](.) for detailed guides
- **Troubleshooting**: See [troubleshooting.md](troubleshooting.md)
- **Issues**: Report bugs at [GitHub Issues](https://github.com/rjwalters/loom/issues)
- **MCP Tools**: Use MCP servers for debugging (see [testing.md](testing.md))

## Summary

You've successfully installed Loom and are ready to start orchestrating AI agents!

**Key Takeaways:**
- ✅ Loom works within git repositories
- ✅ Use GUI for visual management or CLI for headless setup
- ✅ Configuration lives in `.loom/` (partially gitignored)
- ✅ Agents coordinate via GitHub labels
- ✅ Customize roles for your project's needs

**Next:**
- Read [WORKFLOWS.md](../../WORKFLOWS.md) to understand agent coordination
- Review [Git Workflow](git-workflow.md) for development patterns
- Explore [Agent Archetypes](../philosophy/agent-archetypes.md) for role philosophy

Happy orchestrating! 🎭
