# Loom

[![codecov](https://codecov.io/gh/rjwalters/loom/branch/main/graph/badge.svg)](https://codecov.io/gh/rjwalters/loom)
[![GitHub Release](https://img.shields.io/github/v/release/rjwalters/loom?include_prereleases)](https://github.com/rjwalters/loom/releases)

**AI-powered development orchestration using GitHub as the coordination layer.**

Loom spawns AI agents that claim issues, implement features, review PRs, and merge code—all coordinated through GitHub labels. Your only job: write issues, review PRs, merge what you like.

## Quick Start

```bash
# Clone and install to your repository
git clone https://github.com/rjwalters/loom
cd loom
./install.sh /path/to/your/repo

# Start autonomous development
cd /path/to/your/repo
/loom
```

Or download [Loom.app](https://github.com/rjwalters/loom/releases) for the GUI.

## How It Works

```
┌─────────────────────────────────────────────────────────────────┐
│                    Human (Layer 3)                              │
│  Write issues, review PRs, merge what you approve               │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│                    Loom Daemon (Layer 2)                        │
│  /loom - Monitors pipeline, spawns shepherds, generates work    │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│                    Shepherds (Layer 1)                          │
│  /shepherd <issue> - Orchestrates: Curator → Builder → Judge    │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│                    Workers (Layer 0)                            │
│  /builder, /judge, /curator, /doctor - Execute single tasks     │
└─────────────────────────────────────────────────────────────────┘
```

**Label-driven workflow:**
- `loom:issue` → Ready for implementation
- `loom:building` → Being worked on
- `loom:review-requested` → PR ready for review
- `loom:pr` → Approved, ready to merge

See [WORKFLOWS.md](docs/workflows.md) for complete label documentation.

## Features

**Autonomous Orchestration**
- Shell-based shepherd orchestration for deterministic, reliable execution
- Stuck agent detection with automatic kill-and-retry recovery
- Rate limit resilience with exponential backoff
- Activity-based completion detection

**Quality Gates**
- Acceptance criteria verification before PR creation
- Automated code review with `/judge`
- PR conflict resolution with `/doctor`
- Main branch validation with `/auditor`

**Developer Experience**
- Git worktree isolation per issue
- Simplified CLI: `/shepherd 42` or `/shepherd --force 42`
- MCP integration for programmatic control (19 tools)
- Graceful shutdown: `touch .loom/stop-daemon`

## Installation

### Requirements

- macOS (Linux support planned)
- Git repository
- tmux (`brew install tmux`)
- [Claude Code](https://claude.ai/code) for AI agents

### Install Options

**Interactive installer:**
```bash
./install.sh /path/to/your/repo
```

**Direct initialization:**
```bash
./loom-daemon init /path/to/your/repo
```

**GUI application:**
Download from [Releases](https://github.com/rjwalters/loom/releases)

### What Gets Installed

```
your-repo/
├── .loom/
│   ├── config.json      # Terminal configuration
│   ├── roles/           # Agent role definitions
│   └── scripts/         # Helper scripts
├── .claude/commands/    # Slash commands
├── .github/labels.yml   # Workflow labels
├── CLAUDE.md            # AI context document
└── AGENTS.md            # Agent coordination guide
```

## Usage

### Daemon Mode (Fully Autonomous)

```bash
/loom              # Start continuous orchestration
/loom --force      # Aggressive mode (auto-promotes proposals)
```

The daemon monitors your pipeline, spawns shepherds for ready issues, and triggers support roles (architect, hermit, auditor) on schedule.

### Manual Mode

```bash
/shepherd 42       # Orchestrate single issue through full lifecycle
/builder 42        # Implement issue 42
/judge 123         # Review PR #123
/curator 42        # Enhance issue with technical details
/doctor 123        # Fix PR feedback or conflicts
```

### Worktree Workflow

```bash
# Create isolated worktree for issue
./.loom/scripts/worktree.sh 42
cd .loom/worktrees/issue-42

# Work, commit, push
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

## Documentation

| Guide | Description |
|-------|-------------|
| [Quickstart Tutorial](docs/guides/quickstart-tutorial.md) | 10-minute hands-on walkthrough |
| [CLI Reference](docs/guides/cli-reference.md) | Full command documentation |
| [Troubleshooting](docs/guides/troubleshooting.md) | Debug common issues |
| [WORKFLOWS.md](docs/workflows.md) | Label-based coordination |
| [DEVELOPMENT.md](docs/guides/development.md) | Contributing to Loom |

### Architecture

| Document | Description |
|----------|-------------|
| [System Overview](docs/architecture/system-overview.md) | Architecture and data flow |
| [ADR Index](docs/adr/README.md) | Architecture decision records |
| [MCP Tools](docs/mcp/README.md) | Programmatic control interface |

## Agent Roles

| Role | Purpose | Mode |
|------|---------|------|
| `/loom` | System orchestration, work generation | Continuous daemon |
| `/shepherd` | Issue lifecycle orchestration | Per-issue |
| `/builder` | Implement features and fixes | Manual |
| `/judge` | Review pull requests | Autonomous |
| `/curator` | Enhance and organize issues | Autonomous |
| `/architect` | Create architectural proposals | Autonomous |
| `/hermit` | Identify simplification opportunities | Autonomous |
| `/doctor` | Fix PR feedback and conflicts | Manual |
| `/champion` | Evaluate proposals, auto-merge PRs | Autonomous |
| `/auditor` | Validate main branch builds | Autonomous |

## Development

```bash
# Clone and setup
git clone https://github.com/rjwalters/loom
cd loom
pnpm install

# Run development server
pnpm app:dev

# Run tests
cargo test --workspace

# Build release
pnpm app:build
```

See [DEVELOPMENT.md](docs/guides/development.md) for complete guidelines.

## Bootstrap New Projects

```bash
# In the Loom repository
/imagine a CLI tool for managing dotfiles
```

Creates a new GitHub repo with Loom pre-installed and initial roadmap.

## License

MIT License © 2025 [Robb Walters](https://github.com/rjwalters)
