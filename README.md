# Loom 🧵  
**Multi-terminal orchestration for AI-powered development.**

Loom turns **GitHub itself** into the ultimate *vibe coding interface*.  
Each issue, label, and pull request becomes part of a living workflow orchestrated by AI workers that read, write, and review code — all through your existing GitHub repo.

---

## ✨ Vision: GitHub as the Vibe Coding UI

When you run `loom`, GitHub becomes a *living, breathing* coding environment.

- The **README** defines your world — project purpose, tone, and architecture.  
- You create **issues** as natural language prompts.  
- Loom spawns **AI workers** that claim, implement, and review those issues.  
- GitHub itself becomes the **shared whiteboard** for you and your AI collaborators.

Your only job: write issues, read pull requests, and merge what you like.  
Everything else — branching, running tests, managing terminals, tracking progress — happens automatically.

---

## 🧠 Core Concepts

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

---

## 🧩 Architecture Overview

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
````

---

## ☁️ Remote Sandboxes (Long-Term)

The Loom daemon will eventually manage **remote sandboxes** — lightweight ephemeral environments (local VMs, SSH hosts, or cloud containers) for running workers in isolation.

| Future Target                 | Description                                                                   |
| ----------------------------- | ----------------------------------------------------------------------------- |
| **Local Sandboxes**           | Use Docker or Podman for isolated builds/tests.                               |
| **Remote Hosts**              | Deploy workers on LAN machines or cloud VMs via SSH.                          |
| **Ephemeral Cloud Sandboxes** | API-driven one-shot environments spun up per task (e.g. Fly.io, AWS Fargate). |
| **Cluster Coordination**      | Workers register via Unix socket or HTTP heartbeat; daemon balances load.     |

Goal: **scale Loom beyond your laptop** — one repo, many AI workers, distributed across machines.

---

## ⚙️ Label Workflow

Loom uses GitHub labels to coordinate work between different agent roles:

| Label | Created By | Reviewed By | Meaning |
|-------|-----------|-------------|---------|
| (no label) | Anyone | Architect triages | Unreviewed issue |
| `loom:architect-suggestion` | Architect | User accepts | Triaged, awaiting approval |
| `loom:accepted` | User | Curator enhances | Approved, awaiting enhancement |
| `loom:ready` | Curator | Worker implements | Enhanced, ready for work |
| `loom:in-progress` | Worker | Worker completes | Being implemented |
| `loom:blocked` | Worker | User/Worker resolves | Blocked, needs help |
| `loom:review-requested` | Worker | Reviewer reviews | PR ready for review |
| `loom:reviewing` | Reviewer | Reviewer completes | PR under review |

For complete workflow documentation, see [WORKFLOWS.md](WORKFLOWS.md).

---

## 🧵 Example Workflow

1. **Architect Bot** (autonomous, runs every 15 minutes) scans the codebase and creates an unlabeled issue:
   > "Add search functionality to terminal history"

   Then triages it by adding `loom:architect-suggestion` label.

2. **You** review the suggestion and add `loom:accepted` label to approve it.

3. **Curator Bot** (autonomous, runs every 5 minutes) finds the `loom:accepted` issue, adds implementation details, test plans, and code references. Removes `loom:accepted` and adds `loom:ready`.

4. **Worker Bot** (manual or on-demand) finds `loom:ready` issues, claims it by adding `loom:in-progress`, implements the feature, creates a PR with "Closes #X", and adds `loom:review-requested`.

5. **Reviewer Bot** (autonomous, runs every 5 minutes) finds the PR, reviews the code, runs tests, and approves or requests changes. Removes `loom:reviewing` when complete.

6. **You** merge the approved PR. GitHub automatically closes the linked issue.

GitHub shows the whole lifecycle — Loom orchestrates it through labels and autonomous terminals.

---

## 🧰 Tech Stack

**Frontend**

* Tauri (Rust + Web)
* Vanilla TypeScript
* TailwindCSS
* xterm.js

**Backend**

* Rust (daemon)
* tmux for terminal persistence
* Unix domain sockets for IPC
* GitHub REST & GraphQL APIs

**Platform**

* macOS initially
* Linux and remote sandbox support planned

---

## 🚀 Roadmap

**Completed:**
* [x] Multi-terminal GUI (Tauri + xterm.js)
* [x] Terminal configuration with role-based system
* [x] File-based role definitions (`.loom/roles/*.md`)
* [x] Autonomous mode with configurable intervals
* [x] Label-based workflow coordination
* [x] Persistent daemon with tmux
* [x] Linting, formatting, and CI setup

**In Progress:**
* [ ] GitHub issue polling + label state machine
* [ ] Worker spawn automation

**Planned:**
* [ ] PR review loop integration
* [ ] Remote sandbox execution
* [ ] Cost tracking and dashboard
* [ ] Self-improving loop: Loom workers improving Loom

---

## ⚙️ Development Setup

```bash
# Clone the repository
git clone https://github.com/rjwalters/loom.git
cd loom

# Install dependencies
pnpm install

# Terminal 1: Start daemon with live monitoring
pnpm run daemon:dev

# Terminal 2: Start Tauri dev mode
pnpm run tauri:dev
```

**For detailed development workflow, see [DEV_WORKFLOW.md](DEV_WORKFLOW.md).**

### CLI Usage

Loom supports command-line arguments for headless automation and remote development workflows:

```bash
# Launch with a specific workspace
./Loom.app/Contents/MacOS/Loom --workspace /path/to/your/repo

# Short form
./Loom.app/Contents/MacOS/Loom -w /path/to/your/repo
```

**Use Cases**:
- Automated deployment: Launch Loom with a pre-configured workspace on server startup
- CI/CD integration: Run Loom headlessly in containerized environments
- Remote development: Start Loom via SSH with a specific repository path

The app will validate the workspace path and automatically load the configuration from `.loom/config.json` if it exists.

### MCP Servers for Testing and Automation

Loom provides three Model Context Protocol (MCP) servers that enable AI agents like Claude Code to interact with the application programmatically:

- **[mcp-loom-ui](docs/mcp/loom-ui.md)** - Interact with UI, console logs, and workspace state
- **[mcp-loom-logs](docs/mcp/loom-logs.md)** - Access daemon, Tauri, and terminal logs
- **[mcp-loom-terminals](docs/mcp/loom-terminals.md)** - Control terminals via daemon IPC

**Use Cases**:
- Testing factory reset and agent launches
- Monitoring agent activity in real-time
- Debugging terminal and IPC issues
- Automating workspace operations

**Quick Start**:
```bash
# Build MCP servers
pnpm build

# Configure in .mcp.json (already included)
# Use from Claude Code:
mcp__loom-ui__read_console_log({ lines: 100 })
mcp__loom-terminals__list_terminals()
```

**Full documentation**: [docs/mcp/README.md](docs/mcp/README.md)

### Configuring Terminal Roles

After launching Loom, you can configure each terminal with a specific role:

1. Click the **settings icon** (⚙️) next to any terminal in the mini terminal row
2. Choose a role from the dropdown (Worker, Reviewer, Architect, Curator, Issues, or Default)
3. Configure autonomous mode:
   - **Autonomous**: Terminal runs at intervals (e.g., every 5 minutes)
   - **Interval Prompt**: The message sent at each interval (e.g., "Continue working on open tasks")
4. Click **Save** to apply the configuration

**Role Files**: All role definitions are stored in `.loom/roles/` as markdown files with optional JSON metadata. See [defaults/roles/README.md](defaults/roles/README.md) for details on creating custom roles.

### Running Tests

```bash
# Run all workspace tests
cargo test --workspace

# Run daemon integration tests
npm run daemon:test

# Run with verbose output (see logs)
npm run daemon:test:verbose

# Run specific test
cargo test --test integration_basic test_ping_pong -- --nocapture
```

**Requirements**: Tests require `tmux` installed (`brew install tmux` on macOS)

---

## 🪄 Philosophy

> *Like a traditional loom weaves threads into fabric, Loom weaves AI agents into cohesive software systems.*

Loom aims to make **autonomous, self-improving development** natural —
you define goals, and the system builds, reviews, and learns from itself.

---

### 🧩 Architecture Bot (Human-in-the-Loop Design)

In Loom’s future ecosystem, an **Architecture Bot** will run periodically to scan the codebase, documentation, and open issues to surface structural opportunities — not tasks.

It creates new GitHub issues labelled **`suggestion`**, which might include:

- “Refactor terminal session handling into a reusable module”
- “Extract common code between Claude and GPT workers”
- “Add healthcheck endpoints to remote sandboxes”
- “Document the worker orchestration state machine”

These issues are **never acted on automatically**.

They are **owned by the human** — the architect who defines the system’s intent and approves direction.  
The **`suggestion`** label acts as a *safety interlock*:  

- As long as `suggestion` is present, Loom’s Issue Bot will ignore the issue.  
- Once the human removes the label (confirming it’s worth pursuing), the Issue Bot can refine and re-label it as `ready`, enabling the normal worker lifecycle.

This keeps the feedback loop safe and directional:

## 🪪 License

MIT License © 2025 [Robb Walters](https://github.com/rjwalters)


