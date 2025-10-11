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

| Concept | Description |
|----------|-------------|
| **Issue Bot** | Improves user-created issues, applies labels like `ready`, `needs-info`, `blocked`. |
| **Worker Bot** | Claims `ready` issues, creates isolated git worktrees, writes code, opens PRs. |
| **Review Bot** | Reviews open PRs, comments inline, and marks them `approved` or `changes-requested`. |
| **Coordinator** | Monitors the repo and spawns or retires workers dynamically as workload changes. |

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

## ⚙️ Agent State Machine

| Label         | Meaning                                | Next State                       | Responsible Agent |
| ------------- | -------------------------------------- | -------------------------------- | ----------------- |
| `new`         | User-created issue                     | `draft`                          | Issue Bot         |
| `draft`       | Being clarified/refined                | `ready`                          | Issue Bot         |
| `ready`       | Ready for implementation               | `in-progress`                    | Worker Bot        |
| `in-progress` | Being worked on                        | `review`                         | Worker Bot        |
| `review`      | PR created, pending review             | `approved` / `changes-requested` | Review Bot        |
| `approved`    | Passed automated + AI review           | `merged`                         | Human             |
| `blocked`     | Waiting on clarification or dependency | `ready`                          | Issue Bot         |

---

## 🧵 Example Workflow

1. **You** create a new GitHub issue:

   > “Add dark mode toggle to the terminal panel.”

2. **Issue Bot** improves the description, labels it `ready`.

3. **Worker Bot** spins up a sandbox, checks out a new worktree, writes the implementation, commits, and opens a PR.

4. **Review Bot** comments and marks it `approved`.

5. **You** merge the PR. Loom marks the issue `done` and closes the worker session.

GitHub shows the whole lifecycle — but Loom orchestrates it behind the scenes.

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

* [x] Multi-terminal GUI (Tauri + xterm.js)
* [x] Persistent daemon with tmux
* [x] Linting, formatting, and CI setup
* [ ] GitHub issue polling + label state machine
* [ ] Worker spawn automation
* [ ] PR review loop
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

# Configure environment
cp .env.example .env
# Edit with your API keys and workspace path

# Start daemon (Terminal 1)
cd loom-daemon
cargo run

# Start GUI (Terminal 2)
pnpm tauri dev
```

---

## 🪄 Philosophy

> *Like a traditional loom weaves threads into fabric, Loom weaves AI agents into cohesive software systems.*

Loom aims to make **autonomous, self-improving development** natural —
you define goals, and the system builds, reviews, and learns from itself.

---

## 🪪 License

MIT License © 2025 [Robb Walters](https://github.com/rjwalters)


