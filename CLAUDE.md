# Loom - AI Development Context

## Project Overview

**Loom** is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. Think of it as a visual terminal manager where each terminal can be assigned to an AI agent working on different features simultaneously.

### Core Concept

- **Primary Display**: Large view showing the currently selected agent terminal
- **Mini Terminal Row**: Horizontal strip at bottom showing all active agent terminals
- **Workspace Selection**: Git repository workspace picker with validation
- **AI Orchestration**: Each agent terminal works on different features in git worktrees
- **GitHub Coordination**: Agents create PRs, issues serve as task queue

### Current Status

- ✅ Comprehensive terminal management with role-based configuration
- ✅ Daemon architecture with Rust and tmux
- ✅ Claude Code agent integration with autonomous modes
- ✅ Label-based workflow coordination (see [WORKFLOWS.md](WORKFLOWS.md))
- ✅ MCP servers for testing and debugging

## Technology Stack

### Frontend
- **Tauri 1.8.1**: Desktop app framework (Rust backend, web frontend)
- **TypeScript 5.9**: Strict mode enabled for maximum type safety
- **Vite 5**: Fast build tool with hot module replacement
- **TailwindCSS 3.4**: Utility-first CSS with dark mode support
- **Vanilla TS**: No framework overhead, direct DOM manipulation

### Backend
- **Rust**: Tauri backend with IPC commands for git validation, role files, label management
- **tmux**: Terminal multiplexing via loom-daemon
- **Claude Code**: AI agent integration

### Why Vanilla TypeScript?

We deliberately chose vanilla TS over React/Vue/Svelte for performance, learning, simplicity, and control. See [ADR-0002](docs/adr/0002-vanilla-typescript-over-frameworks.md).

## Project Structure

```
loom/
├── src/                          # TypeScript frontend
│   ├── main.ts                   # Entry point, state init, events
│   └── lib/                      # State, config, UI, theme
├── src-tauri/                    # Rust backend (Tauri commands)
│   ├── src/
│   │   ├── main.rs               # Entry point, command registration
│   │   ├── commands/             # Domain-specific command modules
│   │   │   ├── terminal.rs       # Terminal management
│   │   │   ├── workspace.rs      # Workspace operations
│   │   │   ├── config.rs         # Config/state I/O
│   │   │   ├── github.rs         # GitHub integration
│   │   │   └── ...               # 9 modules total, 51 commands
│   │   └── menu.rs               # Menu building
│   └── tauri.conf.json           # Tauri configuration
├── loom-daemon/                  # Rust daemon (terminal management)
├── .loom/                        # Workspace config (gitignored)
│   ├── config.json               # Agent counter, terminal roles
│   └── roles/                    # Custom role definitions
├── defaults/                     # Default configuration templates
│   ├── config.json
│   └── roles/                    # System role templates
├── docs/                         # **Detailed Documentation**
│   ├── guides/                   # Development guides
│   └── workflows/                # Agent workflow docs
└── package.json                  # pnpm scripts
```

## Quick Links to Detailed Guides

### Development Guides

- **[Architecture Patterns](docs/guides/architecture-patterns.md)** - Observer pattern, pure functions, IPC, worktrees
- **[TypeScript Conventions](docs/guides/typescript-conventions.md)** - Strict mode, type safety, pitfalls
- **[Code Quality](docs/guides/code-quality.md)** - Linting, formatting, CI/CD, development workflow
- **[Testing](docs/guides/testing.md)** - Daemon tests, MCP instrumentation, debugging
- **[Git Workflow](docs/guides/git-workflow.md)** - Branch strategy, commits, PR checklist
- **[Common Tasks](docs/guides/common-tasks.md)** - Adding properties, state methods, UI sections
- **[Styling](docs/guides/styling.md)** - TailwindCSS usage, theme system, dark mode

### Architecture Decisions

**📖 See [docs/adr/README.md](docs/adr/README.md) for complete ADR index**

Quick reference:
- [ADR-0001: Observer Pattern for State Management](docs/adr/0001-observer-pattern-state-management.md)
- [ADR-0002: Vanilla TypeScript over Frameworks](docs/adr/0002-vanilla-typescript-over-frameworks.md)
- [ADR-0003: Config vs State File Split](docs/adr/0003-config-state-file-split.md)
- [ADR-0004: Worktree Paths Inside Workspace](docs/adr/0004-worktree-paths-inside-workspace.md)
- [ADR-0006: Label-Based Workflow Coordination](docs/adr/0006-label-based-workflow-coordination.md)

## Essential Patterns Summary

### State Management (Observer Pattern)

```typescript
// src/lib/state.ts - Single source of truth
export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private listeners: Set<() => void> = new Set();

  private notify(): void {
    this.listeners.forEach(cb => cb());
  }

  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }
}
```

**Why?** Decouples state from UI, automatic updates, single source of truth.

### Configuration & Worktrees

- **Config**: `.loom/config.json` (workspace-specific, gitignored)
- **Worktrees**: On-demand creation using `./.loom/scripts/worktree.sh <issue-number>` (or `pnpm worktree` in loom itself)
- **Agents start in main workspace**, create worktrees when claiming issues

### Terminal Roles

Each terminal can be assigned a specialized role from `defaults/roles/`:
- **Builder** (manual, builder.md): Implements features, creates PRs
- **Judge** (autonomous 5min, judge.md): Reviews PRs
- **Curator** (autonomous 5min, curator.md): Enhances issues
- **Architect** (autonomous 15min, architect.md): Creates proposals
- **Hermit** (autonomous 15min, hermit.md): Identifies bloat
- **Healer** (manual, healer.md): Fixes bugs and maintains PRs
- **Guide** (autonomous 15min, guide.md): Prioritizes issues
- **Driver** (manual, driver.md): Plain shell, direct action

See [WORKFLOWS.md](WORKFLOWS.md) and [Agent Archetypes](docs/philosophy/agent-archetypes.md) for complete details.

## Development Workflow

### Before Starting

```bash
# Check you're in the right location
pnpm worktree --check

# If in a worktree for different issue, return to main
cd /Users/rwalters/GitHub/loom
```

### Working on an Issue

```bash
# 1. Claim issue (as Builder)
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:in-progress"

# 2. Create worktree
pnpm worktree 42
cd .loom/worktrees/issue-42

# 3. Implement, test, commit
# ... make your changes ...

# 4. CRITICAL: Run full CI suite locally
pnpm check:ci

# 5. Commit and push
git add -A
git commit -m "Your message"
git push -u origin feature/issue-42

# 6. Create PR
gh pr create --label "loom:review-requested"
```

### Development Modes

- **`pnpm app:dev`**: Hot reload for frontend-only development
- **`pnpm app:preview`**: Full rebuild, use when agents work on Loom codebase (prevents restart loops)
- **`pnpm app:build`**: Production build

**CRITICAL**: Never use `app:dev` when agent terminals work on Loom itself - causes infinite restart loops. See [Code Quality Guide](docs/guides/code-quality.md#self-modification-problem).

## Essential Commands

### Package Scripts (Always use these, NOT cargo/biome directly)

```bash
pnpm check:ci          # Run FULL CI suite (REQUIRED before PR)
pnpm lint              # Biome linting
pnpm format            # Biome formatting
pnpm clippy            # Clippy with exact CI flags
pnpm test              # Run all tests
pnpm app:preview       # Rebuild and launch (recommended)
```

**Why pnpm scripts?** Matches CI exactly. `cargo clippy` directly misses flags like `--all-targets`.

### Git Worktree Helper

```bash
# Portable version (works in any loom-initialized repo)
./.loom/scripts/worktree.sh 42       # Create worktree for issue #42
./.loom/scripts/worktree.sh --check  # Check current worktree status
./.loom/scripts/worktree.sh --help   # Show help

# Shorthand (only works in loom repo itself)
pnpm worktree 42                     # Alias for the script above
```

### GitHub CLI

```bash
gh issue list --label="loom:ready"     # Find ready issues
gh pr list --label="loom:approved"     # Find approved PRs
gh pr review 123 --approve             # Approve PR
```

### Workspace Initialization (Headless Mode)

The `loom-daemon init` command sets up Loom workspaces without requiring the GUI:

```bash
loom-daemon init                       # Initialize current directory
loom-daemon init /path/to/repo         # Initialize specific repository
loom-daemon init --dry-run             # Preview changes without applying
loom-daemon init --force               # Overwrite existing .loom directory
loom-daemon init --defaults ./custom   # Use custom defaults directory
```

**What it does:**
1. Validates target is a git repository
2. Copies `.loom/` configuration from `defaults/`
3. Installs repository scaffolding:
   - `CLAUDE.md` - AI context documentation
   - `AGENTS.md` - Agent workflow guide
   - `.claude/` - Claude Code configuration
   - `.github/` - GitHub labels and workflows
4. Updates `.gitignore` with Loom ephemeral patterns

**Use cases:**
- **CI/CD Integration**: Initialize Loom in deployment pipelines
- **Bulk Setup**: Script initialization across multiple repositories
- **Testing**: Set up test environments with custom defaults
- **Development**: Reset workspace to factory defaults

**Under the Hood** (Implementation details for implementing init-related features):

The initialization process is implemented in `loom-daemon/src/init.rs`:

```rust
pub fn initialize_workspace(
    workspace_path: &str,
    defaults_path: &str,
    force: bool,
) -> Result<(), String>
```

**Key implementation details:**
- **Defaults Resolution**: Tries multiple paths (dev, git root, bundled resources)
- **Idempotent**: Only creates files that don't exist (unless `--force`)
- **Scaffolding**: Copies CLAUDE.md, AGENTS.md, .claude/, .codex/, .github/
- **Gitignore Updates**: Merges ephemeral patterns without duplicates
- **Validation**: Ensures target is a git repository before proceeding

**Testing initialization changes:**
```bash
# Test with dry run
loom-daemon init --dry-run

# Test with custom defaults
mkdir test-defaults
cp -r defaults/* test-defaults/
# Modify test-defaults...
loom-daemon init --force --defaults ./test-defaults /tmp/test-repo

# Verify scaffolding
ls -la /tmp/test-repo/.loom
diff defaults/config.json /tmp/test-repo/.loom/config.json
```

**Common errors and recovery:**
- "Not a git repository": Target lacks `.git` directory - run `git init` first
- ".loom already exists": Use `--force` to overwrite or remove manually
- "Permission denied": Check directory ownership and permissions
- "Defaults not found": Specify path explicitly with `--defaults`

**See complete documentation:**
- [Getting Started Guide](docs/guides/getting-started.md) - Installation walkthrough
- [CLI Reference](docs/guides/cli-reference.md) - Full command syntax and flags
- [CI/CD Setup](docs/guides/ci-cd-setup.md) - Pipeline integration examples

### Enhanced Loom Installation Workflow (Issue #442)

For a streamlined installation experience with GitHub integration, use the automated installation workflow:

```bash
# From Loom repository
cd /path/to/loom
./scripts/install-loom.sh /path/to/target-repo
```

**What it does:**
1. Creates GitHub tracking issue in target repository
2. Creates installation worktree at `.loom/worktrees/issue-{NUMBER}`
3. Runs `loom-daemon init` to install Loom files
4. Syncs GitHub labels from `.github/labels.yml`
5. Creates pull request that closes the tracking issue

**Installation Components:**

The installation includes:
- **Modular Scripts** (`scripts/install/`):
  - `validate-target.sh` - Validates prerequisites (git repo, gh CLI)
  - `create-issue.sh` - Creates tracking issue with Loom version info
  - `create-worktree.sh` - Creates git worktree for installation
  - `sync-labels.sh` - Syncs GitHub workflow labels
  - `create-pr.sh` - Commits changes and creates PR
- **Slash Command** (`.claude/commands/install-loom.md`):
  - Orchestrates the installation process
  - Provides detailed error handling and recovery
  - Launched automatically by `install-loom.sh`
- **Documentation Templates** (`defaults/.loom/`):
  - `CLAUDE.md` - Repository-specific usage guide
  - `AGENTS.md` - Agent workflow documentation
- **GitHub Labels** (`defaults/.github/labels.yml`):
  - Canonical Loom workflow labels
  - Synced via `gh label sync`

**Workflow:**
1. User runs `./scripts/install-loom.sh /target/repo`
2. Script extracts Loom version and commit
3. Launches Claude Code with `/install-loom` command
4. Agent orchestrates all installation steps
5. Creates PR for human review and merge

**Benefits:**
- **Trackable**: GitHub issue documents the installation
- **Reviewable**: PR allows team review before merge
- **Automated**: Minimal manual steps required
- **Version-Stamped**: Documentation includes Loom version info

**See also:**
- `scripts/install-loom.sh` - Main entry point
- `defaults/.claude/commands/install-loom.md` - Orchestration guide
- `defaults/.loom/CLAUDE.md` - Target repo documentation template

## MCP Testing & Debugging

Loom provides MCP servers for AI-powered testing:

- **mcp-loom-ui**: Workspace state, console logs, factory reset
- **mcp-loom-logs**: Daemon/Tauri/terminal logs
- **mcp-loom-terminals**: Terminal management, IPC

**Usage**:
```bash
mcp__loom-ui__read_console_log      # Read browser console
mcp__loom-ui__trigger_force_start   # Start engine
```

See [Testing Guide](docs/guides/testing.md) for complete MCP documentation.

## Structured Logging (Issue #130)

All components use JSON-formatted structured logging:

```typescript
import { Logger } from "./logger";
const logger = Logger.forComponent("my-component");

logger.info("Operation complete", { terminalId, path });
logger.error("Failed to load", error, { workspacePath });
```

**Log locations**:
- Frontend: `~/.loom/console.log`
- Daemon: `~/.loom/daemon.log`
- Terminals: `/tmp/loom-terminal-{id}.out`

**Log rotation**: Automatic rotation when files exceed 10MB (keeps last 10 files: `*.log.1` through `*.log.10`)

**See full documentation**: [docs/guides/common-tasks.md#structured-logging](docs/guides/common-tasks.md#structured-logging) for conventions, querying, and migration guide

## Critical AI Agent Requirements

### Pre-PR Checklist (MANDATORY)

Before creating or updating ANY Pull Request:

```bash
pnpm check:ci  # MUST pass clean before creating PR
```

This runs:
- Biome linting and formatting
- Rust formatting (rustfmt)
- Clippy with all CI flags
- Cargo check
- Frontend build
- All tests

**If this fails, FIX IT before creating the PR.** Do NOT create PRs that fail CI.

### Common Mistakes to Avoid

1. ❌ Running `cargo clippy` directly (misses CI flags)
2. ❌ Using `app:dev` when working on Loom codebase (restart loops)
3. ❌ Forgetting to call `this.notify()` after state changes
4. ❌ Missing dark mode variants on Tailwind classes
5. ❌ Creating nested worktrees

### Worktree Best Practices

- **Always** use `./.loom/scripts/worktree.sh <issue>` (or `pnpm worktree` in loom), never `git worktree` directly
- Agents start in **main workspace**, create worktrees on-demand
- Worktrees named by issue: `.loom/worktrees/issue-42`
- Check status: `./.loom/scripts/worktree.sh --check`

## Resources

### Loom Documentation

- **[Troubleshooting Guide](docs/guides/troubleshooting.md)** - Debug common issues, use MCP tools, and resolve CI failures
- **[API Reference](docs/api/README.md)** - Tauri IPC commands, frontend state API, daemon protocol, and MCP servers
- **[Architecture Overview](docs/architecture/system-overview.md)** - System diagrams, component relationships, and data flow
- **[CONTRIBUTING.md](CONTRIBUTING.md)** - Contribution guidelines, development setup, and workflow

### External Resources

- **Tauri Docs**: https://tauri.app/v1/guides/
- **TypeScript Handbook**: https://www.typescriptlang.org/docs/
- **TailwindCSS Docs**: https://tailwindcss.com/docs
- **GitHub Issues**: Track work and discuss architecture

## Project Philosophy

Loom is built on three philosophical pillars:

1. **[Agent Archetypes](docs/philosophy/agent-archetypes.md)** - Each role (Builder, Judge, Curator, Architect, Hermit, Healer, Guide, Driver) embodies a universal pattern from Tarot's Major Arcana, working in harmony through label-based coordination.

2. **[Working with AI](docs/philosophy/working-with-ai.md)** - Insights from building Loom: the shift from writing code to specifying intent, creating machine-readable debugging surfaces, and the evolution of the programmer's craft.

3. **[Loom Intelligence](docs/philosophy/loom-intelligence.md)** - The vision for Loom as a learning system that gets smarter over time, analyzing agent activity to answer strategic questions about effectiveness, cost, and patterns.

## Maintaining Documentation

- **When adding patterns**: Document in relevant guide under `docs/guides/`
- **When making architectural decisions**: Create ADR in `docs/adr/`
- **When finding pitfalls**: Add to relevant guide
- **When removing code**: Update relevant sections

**CLAUDE.md vs Guides vs ADRs**:
- **CLAUDE.md**: Quick reference, high-level overview, links to details
- **Guides (`docs/guides/`)**: How-to documentation, patterns, workflows
- **ADRs (`docs/adr/`)**: Architectural decisions with context and tradeoffs

---

**For detailed documentation on any topic, see the links above. This file provides quick reference only.**

Last updated: Issue #312 - Split large documentation files for token efficiency
