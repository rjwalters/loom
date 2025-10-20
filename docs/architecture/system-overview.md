# Loom System Architecture

This document provides a visual overview of Loom's architecture, including component relationships, data flow, and key design patterns.

## High-Level Architecture

```mermaid
graph TB
    subgraph "Frontend (Tauri WebView)"
        UI[UI Components<br/>Vanilla TypeScript]
        State[AppState<br/>Observer Pattern]
        IPC[IPC Client<br/>Tauri invoke]
    end

    subgraph "Backend (Rust/Tauri)"
        Commands[Tauri Commands<br/>validate_git_repo, etc.]
        DaemonClient[Daemon Client<br/>Unix Socket]
        FS[Filesystem<br/>Role files, config]
    end

    subgraph "Daemon (loom-daemon)"
        Server[Unix Socket Server<br/>~/.loom/loom-daemon.sock]
        TermMgr[Terminal Manager<br/>Registry + Health Monitor]
        Tmux[tmux Sessions<br/>loom-terminal-*]
    end

    subgraph "External"
        Git[Git/GitHub<br/>Issues, PRs, Labels]
        Claude[Claude Code<br/>AI Agents]
    end

    UI --> State
    State --> IPC
    IPC --> Commands
    Commands --> DaemonClient
    Commands --> FS
    DaemonClient --> Server
    Server --> TermMgr
    TermMgr --> Tmux
    Tmux --> Claude
    Claude --> Git

    style UI fill:#e1f5ff
    style State fill:#b3e5fc
    style IPC fill:#81d4fa
    style Commands fill:#ffe0b2
    style DaemonClient fill:#ffcc80
    style FS fill:#fff9c4
    style Server fill:#c8e6c9
    style TermMgr fill:#a5d6a7
    style Tmux fill:#81c784
    style Git fill:#f8bbd0
    style Claude fill:#f48fb1
```

## Component Details

### Frontend Layer

#### UI Components
- **Location:** `src/main.ts`, `src/lib/ui.ts`
- **Technology:** Vanilla TypeScript + TailwindCSS
- **Responsibilities:**
  - Render terminal cards and workspace selector
  - Handle user interactions
  - Update based on state changes
  - Manage theme (light/dark mode)

#### AppState
- **Location:** `src/lib/state.ts`
- **Pattern:** Observer (Pub/Sub)
- **Responsibilities:**
  - Single source of truth for app state
  - Terminal management (add, remove, update)
  - Workspace path tracking
  - Notify observers on changes
- **Key Feature:** Automatic UI updates via `onChange()` callbacks

#### IPC Client
- **Location:** Throughout `src/` (via `@tauri-apps/api`)
- **Responsibilities:**
  - Bridge frontend to Rust backend
  - Type-safe command invocation
  - Async communication with backend

### Backend Layer

#### Tauri Commands
- **Location:** `src-tauri/src/main.rs`
- **Responsibilities:**
  - Validate git repositories
  - Read role files and metadata
  - Manage GitHub labels
  - Write console logs
  - Communicate with daemon

#### Daemon Client
- **Location:** `src-tauri/src/daemon_client.rs`
- **Responsibilities:**
  - Connect to Unix socket
  - Send/receive JSON messages
  - Handle daemon communication errors

#### Filesystem
- **Locations:**
  - Config: `.loom/config.json`
  - State: `.loom/state.json`
  - Roles: `defaults/roles/`, `.loom/roles/`
  - Logs: `~/.loom/*.log`

### Daemon Layer

#### Unix Socket Server
- **Location:** `loom-daemon/src/ipc.rs`
- **Socket:** `~/.loom/loom-daemon.sock`
- **Protocol:** JSON messages (internally tagged)
- **Responsibilities:**
  - Accept client connections
  - Route messages to handlers
  - Maintain connection pool

#### Terminal Manager
- **Location:** `loom-daemon/src/terminal.rs`
- **Responsibilities:**
  - Create/destroy tmux sessions
  - Maintain terminal registry
  - Send input to terminals
  - Read terminal output
  - Health monitoring

#### tmux Sessions
- **Socket:** `-L loom` (separate from system tmux)
- **Naming:** `loom-terminal-{id}`
- **Persistence:** Sessions survive app restart
- **Cleanup:** Auto-removed when app terminates

## Data Flow

### Terminal Creation Flow

```mermaid
sequenceDiagram
    participant User
    participant UI
    participant State
    participant Tauri
    participant Daemon
    participant tmux

    User->>UI: Click "Add Terminal"
    UI->>State: addTerminal({id, name, ...})
    State->>State: notify() observers
    State->>UI: onChange callback
    UI->>UI: Re-render UI
    UI->>Tauri: invoke('create_terminal')
    Tauri->>Daemon: CreateTerminal message
    Daemon->>tmux: new-session -s loom-terminal-1
    tmux-->>Daemon: Session created
    Daemon-->>Tauri: TerminalCreated response
    Tauri-->>UI: Success
    UI->>State: Update with tmux session ID
```

### Agent Launch Flow

```mermaid
sequenceDiagram
    participant UI
    participant Tauri
    participant Daemon
    participant tmux
    participant Agent
    participant GitHub

    UI->>Tauri: launchAgent(terminalId, role)
    Tauri->>Tauri: Load role file
    Tauri->>Daemon: SendInput("claude ...")
    Daemon->>tmux: send-keys
    tmux->>Agent: Launch Claude Code
    Agent->>Agent: Read role prompt
    Agent->>GitHub: gh issue list --label="loom:ready"
    GitHub-->>Agent: Issue list
    Agent->>Agent: Select oldest issue
    Agent->>GitHub: gh issue edit (claim)
    Agent->>Agent: Implement feature
    Agent->>GitHub: gh pr create
```

### State Update Flow

```mermaid
graph LR
    A[State Change] --> B[state.notify]
    B --> C[Trigger onChange<br/>callbacks]
    C --> D[render]
    D --> E[renderHeader]
    D --> F[renderPrimaryTerminal]
    D --> G[renderMiniTerminals]
    G --> H[setupEventListeners]

    style A fill:#ffcdd2
    style B fill:#f8bbd0
    style C fill:#e1bee7
    style D fill:#d1c4e9
    style E fill:#c5cae9
    style F fill:#bbdefb
    style G fill:#b3e5fc
    style H fill:#b2ebf2
```

## Configuration Architecture

```mermaid
graph TB
    subgraph "Workspace (.loom/)"
        Config[config.json<br/>Persistent Config]
        State[state.json<br/>Runtime State]
        Custom[roles/<br/>Custom Roles]
    end

    subgraph "Defaults"
        DefaultConfig[defaults/config.json<br/>Factory Defaults]
        DefaultRoles[defaults/roles/<br/>System Roles]
    end

    subgraph "Home (~/.loom/)"
        Logs[*.log<br/>Console + Daemon Logs]
        Socket[loom-daemon.sock<br/>Unix Socket]
    end

    DefaultConfig -.->|Fallback| Config
    DefaultRoles -.->|Fallback| Custom
    Config --> State
    Custom --> Config

    style Config fill:#fff9c4
    style State fill:#fff59d
    style Custom fill:#ffeb3b
    style DefaultConfig fill:#e0e0e0
    style DefaultRoles fill:#bdbdbd
    style Logs fill:#c8e6c9
    style Socket fill:#a5d6a7
```

## Git Worktree Architecture

```mermaid
graph TB
    subgraph "Main Workspace"
        Main[Main Branch<br/>Production Code]
    end

    subgraph ".loom/worktrees/"
        WT1[issue-42/<br/>feature/issue-42]
        WT2[issue-84/<br/>feature/issue-84]
        WT3[issue-123/<br/>feature/issue-123]
    end

    subgraph "Agents"
        Agent1[Worker 1<br/>Terminal 1]
        Agent2[Worker 2<br/>Terminal 2]
        Agent3[Worker 3<br/>Terminal 3]
    end

    Main -.->|git worktree add| WT1
    Main -.->|git worktree add| WT2
    Main -.->|git worktree add| WT3

    Agent1 --> WT1
    Agent2 --> WT2
    Agent3 --> WT3

    WT1 -.->|PR merge| Main
    WT2 -.->|PR merge| Main
    WT3 -.->|PR merge| Main

    style Main fill:#e1f5ff
    style WT1 fill:#fff9c4
    style WT2 fill:#fff9c4
    style WT3 fill:#fff9c4
    style Agent1 fill:#c8e6c9
    style Agent2 fill:#c8e6c9
    style Agent3 fill:#c8e6c9
```

## Label-Based Workflow

```mermaid
stateDiagram-v2
    [*] --> Unlabeled: Architect creates
    Unlabeled --> Proposal: Add loom:proposal
    Proposal --> Unlabeled: User approves<br/>(removes label)
    Proposal --> [*]: User rejects<br/>(closes issue)
    Unlabeled --> Ready: Curator enhances<br/>Add loom:ready
    Ready --> InProgress: Worker claims<br/>Add loom:in-progress
    InProgress --> Blocked: Dependencies<br/>Add loom:blocked
    Blocked --> InProgress: Unblock<br/>Remove loom:blocked
    InProgress --> ReviewRequested: PR created<br/>Add loom:review-requested
    ReviewRequested --> Reviewing: Reviewer claims<br/>Add loom:reviewing
    Reviewing --> ReviewRequested: Request changes<br/>Back to loom:review-requested
    Reviewing --> Approved: Approve<br/>Add loom:approved
    Approved --> [*]: Merge PR<br/>Auto-close issue

    note right of Proposal: Blue badge<br/>User decision
    note right of Ready: Green badge<br/>Queue for workers
    note right of InProgress: Yellow badge<br/>Active work
    note right of Blocked: Red badge<br/>Dependencies
    note right of ReviewRequested: Orange badge<br/>Awaiting review
```

## Agent Roles and Responsibilities

```mermaid
graph TB
    subgraph "Autonomous Agents (Interval-based)"
        Architect[Architect<br/>15 min interval<br/>Scan codebase<br/>Create proposals]
        Curator[Curator<br/>5 min interval<br/>Enhance issues<br/>Mark loom:ready]
        Reviewer[Reviewer<br/>5 min interval<br/>Review PRs<br/>Approve/Request changes]
    end

    subgraph "Manual Agents (On-demand)"
        Worker[Worker<br/>Manual trigger<br/>Implement features<br/>Create PRs]
        Issues[Issues Specialist<br/>Manual trigger<br/>Create well-formed issues]
    end

    subgraph "GitHub"
        Repo[Repository<br/>Issues & PRs]
    end

    Architect -->|loom:proposal| Repo
    Curator -->|loom:ready| Repo
    Worker -->|loom:in-progress| Repo
    Worker -->|PR + loom:review-requested| Repo
    Reviewer -->|loom:approved| Repo
    Issues -->|New issues| Repo

    style Architect fill:#e1bee7
    style Curator fill:#c5cae9
    style Reviewer fill:#bbdefb
    style Worker fill:#c8e6c9
    style Issues fill:#fff9c4
    style Repo fill:#ffccbc
```

## Health Monitoring Architecture

```mermaid
graph TB
    Monitor[Health Monitor<br/>1-second interval] --> Check{For each<br/>terminal}
    Check -->|Has session?| HasSession[has_tmux_session]
    HasSession -->|Yes| Healthy[Mark healthy]
    HasSession -->|No| Missing[Mark missing]
    Healthy --> State[Update AppState]
    Missing --> State
    State --> UI[UI Updates<br/>Health indicator]

    subgraph "Terminal Registry"
        Registry[(Terminal IDs<br/>tmux sessions<br/>working dirs)]
    end

    HasSession --> Registry

    style Monitor fill:#e1f5ff
    style Check fill:#fff9c4
    style HasSession fill:#ffe0b2
    style Healthy fill:#c8e6c9
    style Missing fill:#ffcdd2
    style State fill:#b3e5fc
    style UI fill:#81d4fa
    style Registry fill:#fff59d
```

## MCP Server Integration

```mermaid
graph TB
    subgraph "Claude Code"
        CC[Claude Code Session]
    end

    subgraph "MCP Servers"
        UI[mcp-loom-ui<br/>State & Logs]
        Logs[mcp-loom-logs<br/>Log Files]
        Terms[mcp-loom-terminals<br/>Terminal Mgmt]
    end

    subgraph "Loom App"
        Frontend[Frontend<br/>~/.loom/console.log]
        Daemon[Daemon<br/>~/.loom/daemon.log]
        Tmux[tmux Sessions<br/>/tmp/loom-*.out]
    end

    CC --> UI
    CC --> Logs
    CC --> Terms

    UI --> Frontend
    Logs --> Daemon
    Logs --> Tmux
    Terms --> Daemon

    style CC fill:#f48fb1
    style UI fill:#c5cae9
    style Logs fill:#bbdefb
    style Terms fill:#b3e5fc
    style Frontend fill:#fff9c4
    style Daemon fill:#c8e6c9
    style Tmux fill:#a5d6a7
```

## File Structure

```
loom/
├── src/                          # Frontend (TypeScript)
│   ├── main.ts                   # Entry point, event handlers
│   ├── lib/
│   │   ├── state.ts              # AppState (Observer pattern)
│   │   ├── config.ts             # Config file I/O
│   │   ├── ui.ts                 # Pure rendering functions
│   │   ├── theme.ts              # Dark/light mode
│   │   ├── workspace-*.ts        # Workspace lifecycle
│   │   ├── terminal-*.ts         # Terminal lifecycle
│   │   ├── agent-launcher.ts     # Agent initialization
│   │   ├── worktree-manager.ts   # Git worktree operations
│   │   └── health-monitor.ts     # Terminal health checks
│   └── style.css                 # Global styles + Tailwind
│
├── src-tauri/                    # Backend (Rust)
│   ├── src/
│   │   ├── main.rs               # Tauri commands
│   │   └── daemon_client.rs      # Daemon communication
│   ├── tauri.conf.json           # Tauri configuration
│   └── Cargo.toml                # Rust dependencies
│
├── loom-daemon/                  # Daemon (Rust)
│   ├── src/
│   │   ├── main.rs               # Daemon entry point
│   │   ├── ipc.rs                # Unix socket server
│   │   ├── terminal.rs           # Terminal manager
│   │   └── logging.rs            # Structured logging
│   └── Cargo.toml                # Daemon dependencies
│
├── .loom/                        # Workspace config (gitignored)
│   ├── config.json               # Persistent configuration
│   ├── state.json                # Runtime state
│   ├── roles/                    # Custom role definitions
│   └── worktrees/                # Git worktrees
│       ├── issue-42/
│       ├── issue-84/
│       └── issue-123/
│
├── defaults/                     # Default templates
│   ├── config.json               # Factory config
│   └── roles/                    # System role templates
│       ├── builder.md
│       ├── judge.md
│       ├── architect.md
│       └── curator.md
│
├── docs/                         # Documentation
│   ├── guides/                   # How-to guides
│   ├── adr/                      # Architecture decisions
│   ├── mcp/                      # MCP server docs
│   ├── api/                      # API reference
│   └── architecture/             # Architecture diagrams
│
└── mcp-*/                        # MCP server packages
    ├── mcp-loom-ui/
    ├── mcp-loom-logs/
    └── mcp-loom-terminals/
```

## Key Design Patterns

### Observer Pattern (State Management)

```typescript
class AppState {
  private terminals: Map<string, Terminal>
  private listeners: Set<() => void>

  private notify(): void {
    this.listeners.forEach(cb => cb())
  }

  onChange(callback: () => void): () => void {
    this.listeners.add(callback)
    return () => this.listeners.delete(callback)
  }
}
```

**Benefits:**
- Automatic UI updates on state changes
- Decoupled components
- Easy to test

### Pure Functions (Rendering)

```typescript
function renderTerminal(terminal: Terminal): string {
  // Same input → Same output
  return `<div>${terminal.name}</div>`
}
```

**Benefits:**
- Predictable behavior
- Easy to test
- Can be memoized

### Event Delegation

```typescript
// One listener on parent handles all child clicks
parent.addEventListener('click', (e) => {
  const card = e.target.closest('[data-terminal-id]')
  if (card) handleTerminalClick(card.dataset.terminalId)
})
```

**Benefits:**
- Fewer event listeners
- Works with dynamic content
- Better performance

## Technology Stack

- **Frontend:** TypeScript 5.9 (Strict), Vite 5, TailwindCSS 3.4
- **Backend:** Rust (Tauri 1.8.1)
- **Daemon:** Rust, tokio async runtime
- **Terminal:** tmux 3.3+
- **AI:** Claude Code (Anthropic)
- **VCS:** Git 2.37+, GitHub CLI
- **Testing:** Vitest (frontend), cargo test (backend), MCP servers

## See Also

- [API Reference](../api/README.md) - Complete API documentation
- [ADR-0001: Observer Pattern](../adr/0001-observer-pattern-state-management.md) - State management decisions
- [ADR-0008: tmux + Rust Daemon](../adr/0008-tmux-daemon-architecture.md) - Daemon architecture
- [Testing Guide](../guides/testing.md) - MCP testing and debugging
