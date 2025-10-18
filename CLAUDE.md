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
├── src-tauri/                    # Rust backend (IPC commands)
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
- **Worktrees**: On-demand creation using `pnpm worktree <issue-number>`
- **Agents start in main workspace**, create worktrees when claiming issues

### Terminal Roles

Each terminal can be assigned a specialized role from `defaults/roles/`:
- **Worker** (manual): Implements features, creates PRs
- **Reviewer** (autonomous, 5min): Reviews PRs
- **Curator** (autonomous, 5min): Enhances issues
- **Architect** (autonomous, 15min): Creates proposals
- **Critic** (autonomous, 15min): Identifies bloat
- **Triage** (autonomous, 15min): Prioritizes issues

See [WORKFLOWS.md](WORKFLOWS.md) for complete workflow details.

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
# 1. Claim issue (as Worker agent)
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
pnpm worktree 42              # Create worktree for issue #42
pnpm worktree --check         # Check current worktree status
pnpm worktree --help          # Show help
```

### GitHub CLI

```bash
gh issue list --label="loom:ready"     # Find ready issues
gh pr list --label="loom:approved"     # Find approved PRs
gh pr review 123 --approve             # Approve PR
```

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

- **Always** use `pnpm worktree <issue>`, never `git worktree` directly
- Agents start in **main workspace**, create worktrees on-demand
- Worktrees named by issue: `.loom/worktrees/issue-42`
- Check status: `pnpm worktree --check`

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

Loom embraces archetypal roles (Architect, Worker, Reviewer, Critic, Curator) that work in harmony through label-based coordination. Each role has a specific purpose and interval. See [Agent Archetypes](docs/philosophy/agent-archetypes.md) for the philosophical framework.

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
