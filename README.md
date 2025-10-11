# Loom

A multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer.

## Overview

Loom enables you to run multiple AI coding assistants (Claude Code, GPT Codex) in parallel, each working in isolated git worktrees. Workers autonomously claim GitHub issues, implement solutions, and submit pull requests. Your role is to create issues and review/merge PRs - the AI handles everything in between.

## Key Features

- **Multi-terminal interface** with primary view and mini terminal row
- **Persistent sessions** - terminals survive app restarts via tmux
- **AI worker orchestration** - spawn Claude Code workers with custom prompts
- **Git worktree isolation** - each worker operates in its own worktree
- **GitHub integration** - workers claim issues and submit PRs autonomously
- **Real-time status indicators** - track worker states at a glance
- **Dark/light mode** - automatic theme switching
- **Lightweight & fast** - vanilla TypeScript, no framework overhead

## Architecture

```
┌─────────────────┐
│  Tauri GUI      │  ← Vanilla TypeScript + xterm.js
│  (Frontend)     │
└────────┬────────┘
         │ Unix socket
         │
┌────────▼────────┐
│ Loom Daemon     │  ← Rust, survives GUI restarts
│  Terminal Mgmt  │
└────────┬────────┘
         │
    ┌────▼─────┐
    │   tmux   │  ← Session persistence
    └──────────┘
```

## Workflow

1. Developer creates GitHub issues describing desired features/fixes
2. Launch Loom workers via the GUI
3. Workers autonomously:
   - Create git worktrees
   - Claim available issues
   - Implement solutions
   - Run tests
   - Submit pull requests
4. Developer reviews and merges PRs
5. GitHub manages all concurrency and conflict resolution

## Tech Stack

**Frontend:**
- Tauri (Rust + Web)
- Vanilla TypeScript (no framework)
- Vite (build tool)
- TailwindCSS (styling)
- xterm.js (terminal display)

**Backend:**
- Rust (daemon process)
- tmux (terminal session management)
- Unix domain sockets (IPC)

**Platform:**
- macOS only (for now)

## Getting Started

### Prerequisites

- macOS 12.0+
- Node.js 18+
- Rust 1.70+
- Git 2.35+
- tmux (`brew install tmux`)
- Claude API key

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/loom.git
cd loom

# Install dependencies
npm install

# Create .env file
cp .env.example .env
# Edit .env and add your API key and workspace path

# Run daemon (Terminal 1)
cd loom-daemon
cargo run

# Run GUI (Terminal 2)
npm run tauri dev
```

### Configuration

Create a `.env` file in the project root:

```bash
# Required: Your Anthropic API key
ANTHROPIC_API_KEY=sk-ant-...

# Required: Path to your git repository workspace
WORKSPACE_PATH=/Users/yourname/projects/loom

# Optional: GitHub token for issue/PR management
GITHUB_TOKEN=ghp_...
```

## Usage

### Launching a Worker

1. Click the **+** button in the mini terminal row
2. Review/customize the system prompt
3. Click **Launch Worker**
4. Worker starts Claude Code and begins working autonomously

### Monitoring Workers

- **Primary view** - See detailed terminal output
- **Mini terminals** - Quick status overview
- **Status indicators:**
  - 🟢 Idle - Ready for work
  - 🔵 Busy - Actively working
  - 🟡 Needs Input - Blocked on user input
  - 🔴 Error - Something failed
  - ⚪ Stopped - Terminal exited

### Switching Terminals

Click any mini terminal to make it the primary view.

## Development Roadmap

- [x] Tauri application structure
- [x] Dark/light mode support
- [x] Multi-terminal layout
- [x] Daemon architecture with tmux
- [x] Terminal display with xterm.js
- [x] Worker launcher with Claude Code
- [ ] .loom/ directory for configuration
- [ ] Workspace selector
- [ ] Status detection from terminal output
- [ ] Cost tracking
- [ ] Worker templates

## Project Structure

```
loom/
├── loom-daemon/          # Rust daemon (terminal management)
│   └── src/
│       ├── main.rs
│       ├── ipc.rs
│       ├── terminal.rs
│       └── types.rs
├── src-tauri/            # Tauri backend (GUI)
│   └── src/
│       ├── main.rs
│       └── daemon_client.rs
├── src/                  # Frontend (vanilla TypeScript)
│   ├── main.ts
│   ├── style.css
│   └── lib/
│       ├── state.ts
│       ├── ui.ts
│       ├── terminal.ts
│       ├── workers.ts
│       └── theme.ts
├── index.html
├── .env.example
└── README.md
```

## Contributing

This project is designed to be self-improving! Once Issue #5 is complete, Loom workers can submit PRs to enhance Loom itself.

1. Fork the repository
2. Open an issue describing the enhancement
3. Let a Loom worker claim and implement it
4. Review and merge the PR

## License

MIT License - See LICENSE file for details

## Why "Loom"?

Like a traditional loom weaves individual threads into fabric, Loom orchestrates multiple AI workers (threads) into a cohesive development workflow. Multiple git branches and terminal sessions are woven together through autonomous coordination.
