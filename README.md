# Loom

A multi-terminal desktop application that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer.

## Overview

Loom enables you to run multiple AI coding assistants (Claude Code, GPT Codex) in parallel, each working in isolated git worktrees. The application uses GitHub issues as a work queue and pull requests as the review mechanism, with the human developer serving as the orchestrator who creates issues and reviews/merges PRs.

## Key Features

- **Multi-terminal interface** with primary and mini views
- **Git worktree isolation** - each worker operates in its own worktree
- **AI worker management** - spawn Claude Code or GPT Codex workers with custom configurations
- **GitHub integration** - workers claim issues, submit PRs, and perform automated tasks
- **Configurable workers** with system prompts and interval-based execution
- **Real-time status indicators** - track worker states (idle, busy, needs input, error, etc.)
- **Human-in-the-loop workflow** - you define work and approve output

## Architecture

### Worker Types

**Main Workers:**
- Claim open GitHub issues
- Create git worktree for isolated development
- Use AI (Claude Code/Codex) to implement the solution
- Submit pull request when complete

**Periodic Workers:**
- **Issue Triage Bot** - Fleshes out GitHub issues into detailed implementation plans
- **PR Review Bot** - Reviews PRs as they're opened and adds comments
- **Documentation Bot** - Keeps README and CLAUDE_ME files up to date
- Custom interval-based workers with configurable system prompts

### Workflow

1. Developer opens GitHub issues describing desired features/fixes
2. Main workers claim available issues
3. Each worker creates a git worktree and branch
4. AI assistant implements the solution
5. Worker submits PR
6. Developer reviews and merges PRs
7. GitHub manages all concurrency and conflict resolution

## Tech Stack

- **Tauri** - Desktop application framework (Rust + Web)
- **xterm.js** - Terminal emulation
- **Rust** - Backend for terminal management, git operations, GitHub API
- **React/TypeScript** - Frontend UI
- **GitHub API** - Work coordination and code review

## Getting Started

### Prerequisites

- Node.js 18+
- Rust 1.70+
- Git 2.35+ (for worktree support)
- GitHub personal access token with repo permissions

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/loom.git
cd loom

# Install dependencies
npm install

# Run in development mode
npm run tauri dev

# Build for production
npm run tauri build
```

### Configuration

On first launch, you'll configure:
- Active GitHub repository
- GitHub API token
- AI assistant API keys (Claude, OpenAI)
- Default worker configurations

## Usage

### Basic Workflow

1. **Set workspace** - Select the GitHub repository to work on
2. **Add workers** - Click the `+` button to configure and launch worker terminals
3. **Monitor progress** - View worker status in mini terminal views
4. **Review PRs** - Check GitHub for submitted pull requests
5. **Merge completed work** - Approve and merge PRs to incorporate changes

### Worker Configuration

When adding a new worker, configure:
- **Assistant type** - Claude Code or GPT Codex
- **System prompt** - Custom instructions for the AI
- **Execution interval** - For periodic workers (e.g., "every 30 minutes")
- **Worker name** - Identifier for easy tracking

## Development Roadmap

- [ ] Basic Tauri application structure
- [ ] Terminal integration with xterm.js
- [ ] Git worktree management
- [ ] Worker terminal spawning
- [ ] GitHub API integration
- [ ] Status indicator system
- [ ] Worker configuration UI
- [ ] Dashboard view
- [ ] Cost tracking
- [ ] Persistent workspace configuration

## Contributing

This project is designed to be self-improving - workers can submit PRs to enhance themselves! 

1. Fork the repository
2. Open an issue describing the enhancement
3. Let a worker claim it, or implement it yourself
4. Submit a PR
5. Review and merge

## License

MIT License - See LICENSE file for details

## Acknowledgments

Built with the vision of autonomous AI development teams, coordinated through the tools developers already use: git and GitHub.

## Why "Loom"?

Loom weaves multiple threads (git branches, AI workers, terminal sessions) together into a cohesive development workflow. Like a traditional loom creates fabric from individual threads, this tool orchestrates parallel AI workers into a unified codebase.
