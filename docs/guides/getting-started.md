# Getting Started with Loom

This guide will walk you through setting up Loom for the first time, whether you prefer using the GUI application or the headless CLI approach.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Installation Options](#installation-options)
  - [Option 1: GUI Installation (Recommended)](#option-1-gui-installation-recommended)
  - [Option 2: CLI Installation (Headless)](#option-2-cli-installation-headless)
- [First-Time Setup](#first-time-setup)
- [Verifying Your Setup](#verifying-your-setup)
- [Next Steps](#next-steps)
- [Troubleshooting](#troubleshooting)

## Prerequisites

Before installing Loom, ensure you have:

1. **macOS** (Loom is currently macOS-only)
2. **Git repository** - Loom works within git repositories
3. **tmux** - Terminal multiplexer (usually pre-installed on macOS)
4. **Claude Code** - For AI agent integration (optional but recommended)

To verify prerequisites:

```bash
# Check git
git --version

# Check tmux
tmux -V

# Check Claude Code (if using AI agents)
claude --version
```

## Installation Options

Loom supports two installation approaches:

1. **GUI Installation** - Launch the Loom app and select a workspace
2. **CLI Installation** - Use `loom-daemon init` for headless setup

### Option 1: GUI Installation (Recommended)

The GUI provides the easiest getting-started experience with visual feedback and workspace management.

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
   - Assign specialized roles (Builder, Judge, Curator, etc.)

5. **Start Working**
   - Terminals are now ready for manual commands or AI agents
   - See [WORKFLOWS.md](../../WORKFLOWS.md) for agent coordination patterns

### Option 2: CLI Installation (Headless)

Perfect for CI/CD pipelines, headless servers, or scripted setups.

#### Steps

1. **Build the Daemon** (if from source)
   ```bash
   cd /path/to/loom
   cargo build --release -p loom-daemon

   # Binary will be at: target/release/loom-daemon
   ```

2. **Add to PATH** (optional)
   ```bash
   # Option A: Symlink to /usr/local/bin
   ln -s /path/to/loom/target/release/loom-daemon /usr/local/bin/loom-daemon

   # Option B: Add to PATH in shell config
   echo 'export PATH="/path/to/loom/target/release:$PATH"' >> ~/.zshrc
   source ~/.zshrc
   ```

3. **Initialize Your Repository**
   ```bash
   # Navigate to your git repository
   cd /path/to/your/repo

   # Initialize Loom workspace
   loom-daemon init
   ```

4. **Verify Installation**
   ```bash
   # Check that .loom directory was created
   ls -la .loom

   # Should show:
   # - config.json
   # - roles/
   # - README.md
   ```

5. **Review Configuration**
   ```bash
   # Check what was installed
   cat .loom/config.json
   cat CLAUDE.md
   cat AGENTS.md
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
- **Architect** (15 min) - Creates `loom:architect-suggestion` proposals
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
