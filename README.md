# Loom

[![codecov](https://codecov.io/gh/rjwalters/loom/branch/main/graph/badge.svg)](https://codecov.io/gh/rjwalters/loom)
[![GitHub Release](https://img.shields.io/github/v/release/rjwalters/loom?include_prereleases)](https://github.com/rjwalters/loom/releases)
[![Lines of Code](https://raw.githubusercontent.com/rjwalters/loom/ghloc/.ghloc/badge.svg)](https://github.com/rjwalters/loom)

**AI-powered development orchestration using your forge as the coordination layer.**

Loom spawns AI agents that claim issues, implement features, review PRs, and merge code -- all coordinated through labels. Your only job: write issues, review PRs, merge what you like.

**Supported Forges**: GitHub | Gitea — Loom auto-detects your forge from the git remote URL. A ForgeClient abstraction layer makes the workflow identical regardless of forge.

## Quick Start

```bash
# Clone and install to your repository
git clone https://github.com/rjwalters/loom
cd loom
./install.sh /path/to/your/repo

# Start autonomous development on a single issue from Claude Code
cd /path/to/your/repo
# In Claude Code:
/loom:sweep 42
```

For multi-issue autonomous batches, start the spawn loop instead:

```bash
LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start
```

## How It Works

```
┌─────────────────────────────────────────────────────────────────┐
│                    Human (Tier 3)                               │
│  Write issues, review PRs, merge what you approve               │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│        Tier 2: Spawn loop + GitHub Actions cron                 │
│  spawn-loop.sh claims ready issues, detaches per-issue sweeps   │
│  .github/workflows/loom-*.yml runs support roles on cron        │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│        Tier 1: /loom:sweep <issue>                              │
│  Single-issue lifecycle: Curator → Builder → Judge → Doctor →   │
│  Merge. Checkpoints survive crashes.                            │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────────┐
│                    Workers (Tier 0)                             │
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
- Signal-based shepherd IPC for deterministic, reliable execution
- Stuck agent detection with automatic kill-and-retry recovery
- Rate limit resilience with exponential backoff
- Activity-based completion detection

**Quality Gates**
- Acceptance criteria verification before PR creation
- Automated code review with `/judge`
- PR conflict resolution with `/doctor`
- Main branch validation with `/auditor`

**Forge-Agnostic**
- Works with GitHub and Gitea out of the box
- Auto-detects forge from git remote URL
- ForgeClient abstraction with 21 methods
- Forge-neutral caching layer for API efficiency

**Developer Experience**
- Git worktree isolation per issue
- Simple slash command: `/loom:sweep 42` runs a single issue end-to-end
- MCP integration for programmatic control (19 tools)
- Graceful shutdown: `touch .loom/stop-spawn-loop`

## Forge Support

Loom's ForgeClient abstraction layer provides a unified interface across forges. All orchestration features — label-driven workflows, issue claiming, PR review, auto-merge — work identically on both platforms.

| Feature | GitHub | Gitea |
|---------|--------|-------|
| Label-based workflow | Yes | Yes |
| Issue/PR operations | Yes | Yes |
| CI status checks | Yes | Yes (Actions API + commit status) |
| Auto-merge | Yes (merge queue) | Yes (poll-and-merge fallback) |
| Branch protection | Yes | Yes |
| Authentication | `gh auth login` or `GH_TOKEN` | `GITEA_TOKEN` or `FORGE_TOKEN` |
| Forge detection | Automatic from remote URL | Automatic from remote URL |

See [Forge Authentication](.loom/docs/forge-authentication.md) for setup details.

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

### What Gets Installed

```
your-repo/
├── .loom/
│   ├── config.json      # Terminal configuration
│   ├── roles/           # Agent role definitions
│   └── scripts/         # Helper scripts
├── .claude/commands/loom/  # Slash commands
├── .github/labels.yml   # Workflow labels
└── CLAUDE.md            # AI context document
```

## Usage

### Single-Issue Mode

To orchestrate one issue end-to-end from inside Claude Code:

```text
/loom:sweep 42          # Curator → Builder → Judge → Doctor → Merge
/loom:sweep --prs 123   # PR-set mode: Judge / Doctor → Judge / Merge from an open-PR set
```

From a script:

```bash
claude -p "/loom:sweep 42" --dangerously-skip-permissions
```

Sweep is self-contained — there is no separate daemon to start. Checkpoints under `.loom/sweep-checkpoint/` survive crashes; restarting the sweep resumes from the last completed phase.

### Multi-Issue Mode (spawn loop)

For autonomous batches that claim ready issues continuously:

```bash
LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start
./.loom/scripts/spawn-loop.sh status
./.loom/scripts/spawn-loop.sh stop                  # or: touch .loom/stop-spawn-loop
```

The spawn loop polls `loom:issue`, atomically claims ready items, and detaches one `/loom:sweep N` child per issue (up to `MAX_PARALLEL`, default 3). Each spawn picks its own OAuth token via `spawn-claude.sh` for multi-account rotation. The loop has no work-generation triggers — see the [GitHub Actions cron workflows](.github/workflows/) for periodic Champion / Curator / Judge / Auditor / Guide ticks (Phase 2a, opt-in per workflow).

### Individual Agent Commands

Run worker agents directly (no daemon required):

```bash
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
| [ADR Index](docs/adr/README.md) | Architecture decision records |
| [MCP Tools](docs/mcp/README.md) | Programmatic control interface |

## Agent Roles

| Role | Purpose | Mode |
|------|---------|------|
| `/loom:sweep` | Single-issue lifecycle orchestration (Curator → Merge) | Per-issue |
| `./.loom/scripts/spawn-loop.sh` | Multi-issue batch claimer (Tier 2) | Continuous, opt-in |
| `/builder` | Implement features and fixes | Manual |
| `/judge` | Review pull requests | Cron via GH Actions |
| `/curator` | Enhance and organize issues | Cron via GH Actions |
| `/architect` | Create architectural proposals | Manual (cadence #3381) |
| `/hermit` | Identify simplification opportunities | Manual (cadence #3381) |
| `/doctor` | Fix PR feedback and conflicts | Manual |
| `/champion` | Evaluate proposals, auto-merge PRs | Cron via GH Actions |
| `/auditor` | Validate main branch builds | Cron via GH Actions |

## Development

```bash
# Clone and setup
git clone https://github.com/rjwalters/loom
cd loom

# Run the daemon in dev mode
./scripts/dev-daemon.sh

# Run tests
cargo test --workspace

# Build release daemon
cargo build --package loom-daemon --release
```

See [DEVELOPMENT.md](docs/guides/development.md) for complete guidelines.

## Bootstrap New Projects

```bash
# In the Loom repository
/imagine a CLI tool for managing dotfiles
```

Creates a new GitHub repo with Loom pre-installed and initial roadmap.

## Built with Loom

If your project was built with Loom, you can add a badge to your README:

[![Built with Loom](https://img.shields.io/badge/Built_with-Loom-blue?logo=github)](https://github.com/rjwalters/loom)

```markdown
[![Built with Loom](https://img.shields.io/badge/Built_with-Loom-blue?logo=github)](https://github.com/rjwalters/loom)
```

## License

MIT License © 2025 [Robb Walters](https://github.com/rjwalters)
