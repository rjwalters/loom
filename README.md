# Loom

[![codecov](https://codecov.io/gh/rjwalters/loom/branch/main/graph/badge.svg)](https://codecov.io/gh/rjwalters/loom)
[![GitHub Release](https://img.shields.io/github/v/release/rjwalters/loom?include_prereleases)](https://github.com/rjwalters/loom/releases)

> AI-powered development orchestration through multi-terminal workspace management

**Multi-terminal workspace where AI agents embody distinct roles—Worker, Curator, Architect, Reviewer, Critic, Fixer—weaving chaos into creation.**

Loom turns **GitHub itself** into the ultimate development interface. Each issue, label, and pull request becomes part of a living workflow orchestrated by AI workers that read, write, and review code—all through your existing GitHub repo.

---

## Download

**[Download Loom.app from Releases](https://github.com/rjwalters/loom/releases)**

| Platform | Download |
|----------|----------|
| macOS (Apple Silicon) | [Loom_x.x.x_aarch64.dmg](https://github.com/rjwalters/loom/releases/latest) |
| macOS (Intel) | [Loom_x.x.x_x64.dmg](https://github.com/rjwalters/loom/releases/latest) |

> **Note**: Loom is currently macOS-only. Linux support is planned.

After downloading:
1. Open the DMG file
2. Drag `Loom.app` to your Applications folder
3. Launch Loom and select your workspace

---

## Quick Start

### For End Users (Using Loom)

Skip the prerequisites and get Loom installed in your repository:

```bash
# Option 1: Interactive Install (Easiest)
# Clone Loom and run the install script
git clone https://github.com/rjwalters/loom
cd loom
./install.sh /path/to/your/repo

# Option 2: Direct CLI Initialization
# Download and run loom-daemon to initialize your repository
./loom-daemon init /path/to/your/repo

# Option 3: GUI Application
# Download Loom.app from releases and open with your workspace
open -a Loom --args --workspace /path/to/your/repo
```

**What this does**: Creates `.loom/`, `CLAUDE.md`, `AGENTS.md`, `.claude/`, and `.github/` in your repository.

**Next steps**: [10-Minute Quickstart Tutorial](docs/guides/quickstart-tutorial.md) - Learn the complete workflow hands-on.

### For Contributors (Building Loom)

Want to contribute to Loom itself?

```bash
git clone https://github.com/rjwalters/loom
cd loom
pnpm install
pnpm app:dev
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for complete development setup and guidelines.

### Repository Maintenance

Two convenience scripts are available at the repository root:

**Installation Helper** (`./install.sh`):
```bash
# Interactive installer with guided prompts
./install.sh

# Install to specific repository
./install.sh /path/to/your/repo
```

Provides two installation workflows:
- **Quick Install**: Direct installation via `loom-daemon init`
- **Full Install**: Creates GitHub issue, worktree, and PR for review

**Cleanup Helper** (`./clean.sh` in Loom repo, `./.loom/scripts/clean.sh` in target repos):
```bash
# In Loom repository
./clean.sh --dry-run

# In target repositories (after installation)
./.loom/scripts/clean.sh --dry-run

# Options
--deep       # Include build artifacts (target/, node_modules/)
--dry-run    # Preview what would be cleaned
```

Safely removes:
- Orphaned worktrees and stale branches
- Loom tmux sessions
- Build artifacts (with `--deep`)
- **Installed automatically** to `.loom/scripts/` in target repositories

---

## Before You Install

### What Loom Does

Loom transforms your repository into an AI-orchestrated workspace where agents coordinate through GitHub issues, PRs, and labels. Each terminal can embody a specialized role (Worker, Curator, Architect, Reviewer) working autonomously or on-demand.

### What Gets Installed

Running `loom-daemon init` creates these files in your repository:

**Configuration (Commit these)**:
- `.loom/config.json` - Terminal settings and role assignments
- `.loom/roles/` - Custom agent role definitions (optional)
- `.loom/scripts/` - Helper scripts (worktree.sh, clean.sh)

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

---

## Installation

### Prerequisites

**For Using Loom** (end users):
- macOS (Linux support planned)
- Git repository
- tmux (usually pre-installed on macOS)
- Claude Code (optional, for AI agents)

**For Developing Loom** (contributors):
- All of the above, plus:
- Rust (via rustup)
- Node.js v18+
- pnpm
- System dependencies (xcode-select on macOS, libwebkit2gtk on Linux)

See [Prerequisites Details](#detailed-prerequisites) for installation instructions.

### Installation Options

#### Option 1: Download Binary (Easiest)

```bash
# Download latest release
curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
chmod +x loom-daemon

# Initialize your repository
./loom-daemon init /path/to/your/repo
```

#### Option 2: Build from Source

```bash
# Clone Loom repository
git clone https://github.com/rjwalters/loom
cd loom

# Build daemon
pnpm daemon:build

# Initialize your repository
./target/release/loom-daemon init /path/to/your/repo
```

#### Option 3: Interactive Install Script

```bash
# Use the install helper (validates and confirms before applying)
./install.sh

# Or specify target repository directly
./install.sh /path/to/your/repo
```

The install script provides two workflows:
- **Quick Install (Option 1)**: Direct installation using `loom-daemon init`
- **Full Install (Option 2)**: Automated workflow with GitHub issue, worktree, and PR creation

Both options include:
- Git repository validation before making changes
- Preview of what will be created
- Confirmation prompts at each step
- Clear error messages if prerequisites missing

For programmatic installation, use:
```bash
# Automated full workflow (no prompts)
./scripts/install-loom.sh /path/to/your/repo
```

#### Option 4: GUI Application

1. Download `Loom.app` from [releases](https://github.com/rjwalters/loom/releases)
2. Move to Applications folder
3. Open Loom.app
4. Choose workspace via file picker

### Initialization Options

The `loom-daemon init` command supports several flags:

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

### Common Installation Issues

| Error | Cause | Solution |
|-------|-------|----------|
| "Not a git repository" | No `.git` directory found | Run `git init` first or use correct path |
| ".loom already exists" | Workspace already initialized | Use `--force` to overwrite or skip if already set up |
| "Permission denied" | Insufficient write permissions | Check directory ownership: `ls -la` |
| "Defaults directory not found" | Cannot locate defaults | Specify explicitly: `--defaults /path/to/loom/defaults` |

For more troubleshooting: [Troubleshooting Guide](docs/guides/troubleshooting.md)

---

## Next Steps

**New to Loom?** Start with the [10-Minute Quickstart Tutorial](docs/guides/quickstart-tutorial.md)

This hands-on walkthrough shows you how to:
- ✅ Create and curate an issue
- ✅ Implement a feature with worktrees
- ✅ Create and review a pull request
- ✅ Understand the complete label workflow

**Want to dive deeper?**
- [Getting Started Guide](docs/guides/getting-started.md) - Complete installation walkthrough
- [CLI Reference](docs/guides/cli-reference.md) - Full command documentation
- [Agent Workflows](WORKFLOWS.md) - How agents coordinate through labels
- [Configuring Terminal Roles](#configuring-terminal-roles) - Customize agent behavior

---

## Documentation

### Essential Guides
- **[Quickstart Tutorial](docs/guides/quickstart-tutorial.md)** - 10-minute hands-on walkthrough
- **[Getting Started](docs/guides/getting-started.md)** - Complete installation details
- **[Troubleshooting Guide](docs/guides/troubleshooting.md)** - Debug common issues
- **[CLI Reference](docs/guides/cli-reference.md)** - Full command documentation
- **[API Reference](docs/api/README.md)** - Complete API documentation

### Development Guides
- **[DEVELOPMENT.md](DEVELOPMENT.md)** - Development setup and best practices
- **[DEV_WORKFLOW.md](DEV_WORKFLOW.md)** - Detailed development workflow
- **[WORKFLOWS.md](WORKFLOWS.md)** - Agent coordination via GitHub labels
- **[CONTRIBUTING.md](CONTRIBUTING.md)** - Contribution guidelines

### Architecture
- **[Architecture Overview](docs/architecture/system-overview.md)** - System design and data flow
- **[ADR Index](docs/adr/README.md)** - Architecture decision records

---

## What is Loom?

### Vision: GitHub as the Vibe Coding UI

When you run `loom`, GitHub becomes a *living, breathing* coding environment.

- The **README** defines your world—project purpose, tone, and architecture.
- You create **issues** as natural language prompts.
- Loom spawns **AI workers** that claim, implement, and review those issues.
- GitHub itself becomes the **shared whiteboard** for you and your AI collaborators.

Your only job: write issues, read pull requests, and merge what you like.
Everything else—branching, running tests, managing terminals, tracking progress—happens automatically.

### Core Concepts

**Current Implementation:**

Loom provides a **multi-terminal GUI** with configurable AI worker roles. Each terminal can be assigned a role that defines its behavior and automation level.

| Concept | Description | Status |
|---------|-------------|--------|
| **Terminal Roles** | Define specialized behaviors for each terminal (Worker, Reviewer, Architect, Curator, Issues) | ✅ Implemented |
| **File-based Configuration** | Role definitions stored as `.md` files in `.loom/roles/` with optional `.json` metadata | ✅ Implemented |
| **Label-based Workflow** | GitHub labels coordinate work between different agent types | ✅ Implemented |
| **Autonomous Mode** | Terminals can run at intervals (e.g., every 5 minutes) with configured prompts | ✅ Implemented |
| **Multi-terminal GUI** | Tauri-based app with xterm.js terminals, theme support, and persistent state | ✅ Implemented |

See [WORKFLOWS.md](WORKFLOWS.md) for detailed documentation of the agent coordination system.

### Architecture Overview

```text
┌────────────────────────┐
│        Loom GUI        │  ← Tauri + Vanilla TypeScript + xterm.js
│  Multi-terminal view   │
└────────────┬───────────┘
             │ Unix socket
┌────────────▼───────────┐
│      Loom Daemon       │  ← Rust backend
│  Worker orchestration  │
│  Local + Remote exec   │
└────────────┬───────────┘
             │
       ┌─────▼──────┐
       │   tmux     │  ← Local terminal persistence
       └────────────┘
             │
       ┌─────▼────────────┐
       │  GitHub API      │  ← Issues, PRs, Labels = orchestration protocol
       └──────────────────┘
```

### Tech Stack

**Frontend**:
- Tauri (Rust + Web)
- Vanilla TypeScript
- TailwindCSS
- xterm.js

**Backend**:
- Rust (daemon)
- tmux for terminal persistence
- Unix domain sockets for IPC
- GitHub REST & GraphQL APIs

**Platform**:
- macOS initially
- Linux and remote sandbox support planned

---

## Label Workflow

Loom uses GitHub labels to coordinate work between different agent roles:

### Issue Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:architect` | 🔵 Blue | Architect | Suggestion awaiting approval |
| `loom:hermit` | 🔵 Blue | Critic | Removal/simplification awaiting approval |
| `loom:curated` | 🟠 Orange | Curator | Enhanced, awaiting human approval |
| `loom:issue` | 🟢 Green | Human | Approved for Worker to implement |
| `loom:building` | 🟡 Amber | Worker | Being implemented |
| `loom:blocked` | 🔴 Red | Worker | Blocked, needs help |
| `loom:urgent` | 🔴 Dark Red | Triage | High priority (max 3) |

### PR Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:review-requested` | 🟢 Green | Worker/Fixer | PR ready for Reviewer |
| `loom:changes-requested` | 🟡 Amber | Reviewer | PR needs fixes from Fixer |
| `loom:pr` | 🔵 Blue | Reviewer | Approved, ready to merge |

For complete workflow documentation, see [WORKFLOWS.md](WORKFLOWS.md).

### Example Workflow

1. **Architect Bot** (autonomous, runs every 15 minutes) scans the codebase and creates an issue with `loom:architect` label:
   > "Add search functionality to terminal history"

2. **You** review the proposal. Remove `loom:architect` to approve it for curation (or close the issue to reject).

3. **Curator Bot** (autonomous, runs every 5 minutes) finds the approved issue, adds implementation details, test plans, and code references. Marks it as `loom:curated`.

4. **You** review the curated issue and explicitly add `loom:issue` label to approve it for implementation.

5. **Worker Bot** (manual or on-demand) finds `loom:issue` issues, claims it by adding `loom:building`, implements the feature, creates a PR with "Closes #X", and adds `loom:review-requested`.

6. **Reviewer Bot** (autonomous, runs every 5 minutes) finds the PR, reviews the code, runs tests, and either:
   - Approves: adds `loom:pr` (ready for you to merge)
   - Requests changes: adds `loom:changes-requested` (for Fixer bot to address)

7. **You** merge the approved PR with `loom:pr` label. GitHub automatically closes the linked issue.

GitHub shows the whole lifecycle—Loom orchestrates it through labels and autonomous terminals with explicit human approval gates.

---

## Configuring Terminal Roles

After launching Loom, you can configure each terminal with a specific role:

1. Click the **settings icon** (⚙️) next to any terminal in the mini terminal row
2. Choose a role from the dropdown (Worker, Reviewer, Architect, Curator, Issues, or Default)
3. Configure autonomous mode:
   - **Autonomous**: Terminal runs at intervals (e.g., every 5 minutes)
   - **Interval Prompt**: The message sent at each interval (e.g., "Continue working on open tasks")
4. Click **Save** to apply the configuration

**Role Files**: All role definitions are stored in `.loom/roles/` as markdown files with optional JSON metadata. See [defaults/roles/README.md](defaults/roles/README.md) for details on creating custom roles.

---

## CLI Usage

### Workspace Initialization (Headless Mode)

Initialize a Loom workspace without launching the GUI app—perfect for CI/CD, headless servers, or manual orchestration:

```bash
# Initialize current directory
loom-daemon init

# Initialize specific repository
loom-daemon init /path/to/your/repo

# Preview changes without applying them
loom-daemon init --dry-run

# Overwrite existing .loom directory
loom-daemon init --force

# Custom defaults directory
loom-daemon init --defaults ./custom-defaults
```

**What Gets Installed**:
- `.loom/` - Configuration directory with terminal roles and settings
- `CLAUDE.md` - AI context documentation for Claude Code
- `AGENTS.md` - Agent workflow and coordination guide
- `.claude/` - Claude Code slash commands and configuration
- `.codex/` - Codex configuration (if available)
- `.github/` - GitHub workflow templates and label definitions
- `.gitignore` - Updated with Loom ephemeral patterns

**Use Cases**:
- **Manual Orchestration**: Set up Loom in a repo and run agents manually (`claude --role builder`)
- **CI/CD Pipelines**: Initialize Loom as part of your build/deploy process
- **Headless Servers**: Install Loom configuration without GUI dependencies
- **Bulk Setup**: Script initialization across multiple repositories

**Comprehensive Documentation**:
- **[Getting Started Guide](docs/guides/getting-started.md)** - Complete installation walkthrough
- **[CLI Reference](docs/guides/cli-reference.md)** - Full command documentation with all flags and exit codes
- **[CI/CD Setup](docs/guides/ci-cd-setup.md)** - Integration examples for GitHub Actions, GitLab CI, Jenkins, and more
- **[Troubleshooting](docs/guides/troubleshooting.md#initialization-issues)** - Debug initialization failures

### Launching the GUI

Loom supports command-line arguments for headless automation and remote development workflows:

```bash
# Launch with a specific workspace
./Loom.app/Contents/MacOS/Loom --workspace /path/to/your/repo

# Short form
./Loom.app/Contents/MacOS/Loom -w /path/to/your/repo
```

**Use Cases**:
- Automated deployment: Launch Loom with a pre-configured workspace on server startup
- Remote development: Start Loom via SSH with a specific repository path

The app will validate the workspace path and automatically load the configuration from `.loom/config.json` if it exists.

### MCP Server for Testing and Automation

Loom provides a unified MCP (Model Context Protocol) server (`mcp-loom`) that enables AI agents like Claude Code to interact with the application programmatically:

- **[Log Tools](docs/mcp/loom-logs.md)** - Access daemon, Tauri, and terminal logs
- **[UI Tools](docs/mcp/loom-ui.md)** - Interact with UI, console logs, and workspace state
- **[Terminal Tools](docs/mcp/loom-terminals.md)** - Control terminals via daemon IPC

**Use Cases**:
- Testing factory reset and agent launches
- Monitoring agent activity in real-time
- Debugging terminal and IPC issues
- Automating workspace operations

**Quick Start**:
```bash
# Build MCP server
cd mcp-loom && npm run build && cd ..

# Configure in .mcp.json (already included)
# Use from Claude Code:
mcp__loom__read_console_log({ lines: 100 })
mcp__loom__list_terminals()
```

**Full documentation**: [docs/mcp/README.md](docs/mcp/README.md)

---

## Running Tests

```bash
# Run all workspace tests
cargo test --workspace

# Run daemon integration tests
pnpm run daemon:test

# Run with verbose output (see logs)
pnpm run daemon:test:verbose

# Run specific test
cargo test --test integration_basic test_ping_pong -- --nocapture
```

**Requirements**: Tests require `tmux` installed (`brew install tmux` on macOS)

---

## Roadmap

**Completed:**
- [x] Multi-terminal GUI (Tauri + xterm.js)
- [x] Terminal configuration with role-based system
- [x] File-based role definitions (`.loom/roles/*.md`)
- [x] Autonomous mode with configurable intervals
- [x] Label-based workflow coordination
- [x] Persistent daemon with tmux
- [x] Linting, formatting, and CI setup

**In Progress:**
- [ ] GitHub issue polling + label state machine
- [ ] Worker spawn automation

**Planned:**
- [ ] PR review loop integration
- [ ] Remote sandbox execution
- [ ] Cost tracking and dashboard
- [ ] Self-improving loop: Loom workers improving Loom

---

## Remote Sandboxes (Long-Term)

The Loom daemon will eventually manage **remote sandboxes**—lightweight ephemeral environments (local VMs, SSH hosts, or cloud containers) for running workers in isolation.

| Future Target                 | Description                                                                   |
| ----------------------------- | ----------------------------------------------------------------------------- |
| **Local Sandboxes**           | Use Docker or Podman for isolated builds/tests.                               |
| **Remote Hosts**              | Deploy workers on LAN machines or cloud VMs via SSH.                          |
| **Ephemeral Cloud Sandboxes** | API-driven one-shot environments spun up per task (e.g. Fly.io, AWS Fargate). |
| **Cluster Coordination**      | Workers register via Unix socket or HTTP heartbeat; daemon balances load.     |

Goal: **scale Loom beyond your laptop**—one repo, many AI workers, distributed across machines.

---

## Philosophy

> *Like a traditional loom weaves threads into fabric, Loom weaves AI agents into cohesive software systems.*

Loom aims to make **autonomous, self-improving development** natural—
you define goals, and the system builds, reviews, and learns from itself.

### The Archetypal System

Each terminal can embody one of six archetypal forces (see [Agent Archetypes](docs/philosophy/agent-archetypes.md)):

- 🔮 **Worker** (The Magician) - Transforms ideas into reality
- 📚 **Curator** (The High Priestess) - Refines chaos into clarity
- 🏛️ **Architect** (The Emperor) - Envisions structure and design
- ⚖️ **Reviewer** (Justice) - Maintains quality through discernment
- 🔍 **Critic** (The Hermit) - Questions to find truth
- 🔧 **Fixer** (The Hanged Man) - Heals what is broken

*Like the Tarot's Major Arcana or Jung's archetypes, each role represents a universal pattern in software development. When working in harmony, they transform chaos into creation.*

### Architecture Bot (Human-in-the-Loop Design)

In Loom's future ecosystem, an **Architecture Bot** will run periodically to scan the codebase, documentation, and open issues to surface structural opportunities—not tasks.

It creates new GitHub issues labelled **`loom:architect`**, which might include:

- "Refactor terminal session handling into a reusable module"
- "Extract common code between Claude and GPT workers"
- "Add healthcheck endpoints to remote sandboxes"
- "Document the worker orchestration state machine"

These issues are **never acted on automatically**.

They are **owned by the human**—the architect who defines the system's intent and approves direction.
The **`loom:architect`** label acts as a *safety interlock*:

- As long as `loom:architect` is present, the Curator Bot will ignore the issue.
- Once the human removes the label (confirming it's worth pursuing), the Curator Bot can refine and re-label it as `loom:curated`.
- The human must then explicitly add `loom:issue` to approve it for implementation, enabling the normal Worker lifecycle.

This keeps the feedback loop safe and directional with two explicit human approval gates.

---

## Detailed Prerequisites

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

   **Alternative:** Download from https://www.rust-lang.org/tools/install

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

   See [Tauri v2 Prerequisites](https://v2.tauri.app/start/prerequisites/) for other platforms.

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

   # Linux
   # See https://cli.github.com/ for installation instructions

   # Authenticate
   gh auth login
   ```

### Verify Your Setup

Run these commands to verify all prerequisites are installed:

```bash
# Check Rust
rustc --version && cargo --version

# Check Node.js
node --version

# Check pnpm
pnpm --version

# Check GitHub CLI (optional)
gh --version
```

If all commands succeed, you're ready to proceed!

---

## License

MIT License © 2025 [Robb Walters](https://github.com/rjwalters)
