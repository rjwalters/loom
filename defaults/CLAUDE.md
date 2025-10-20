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

- ✅ Issue #1: Basic Tauri setup with TypeScript, TailwindCSS, dark/light theme
- ✅ Issue #2: Layout structure with agent management and workspace selection
- ✅ Issue #3: Daemon architecture with Rust and tmux
- ✅ Issue #4: Terminal display with xterm.js
- ✅ Issue #5: Worker launcher with Claude Code
- ✅ Issue #8: Comprehensive linting, formatting, and CI/CD setup
- ✅ Issue #13: Daemon integration tests with full IPC coverage
- ⏳ Issue #6: .loom/ directory configuration (planned)
- ⏳ Issue #7: Workspace selector improvements (planned)

**Current Work**: Testing and improving factory reset reliability (Issue #84)
- **Status**: 60% success rate (3/5 worker terminals launching Claude Code successfully)
- **Fixes Implemented**: Retry mechanism with exponential backoff (2s, 4s, 6s) + increased worktree command delay (100ms → 300ms)
- **Next**: Restart app and Claude Code to test improvements, aiming for 100% success rate
- **Files Changed**: `src/lib/agent-launcher.ts`, `src/lib/worktree-manager.ts`

### Recent Features

**Issue #19: Terminal Configuration System**
- **Role-based Terminals**: Each terminal can be assigned a specialized role (Worker, Reviewer, Architect, Curator, Issues, Default)
- **File-based Configuration**: Role definitions stored as `.md` files in `.loom/roles/` with optional `.json` metadata
- **Autonomous Mode**: Terminals can run at intervals (e.g., every 5 minutes) with configured prompts
- **Terminal Settings Modal**: Configure role, worker type, interval, and prompts via UI
- **Label-based Workflow**: GitHub labels coordinate work between different agent types (see [WORKFLOWS.md](WORKFLOWS.md))

**Issue #2: Multi-terminal Layout**
- **Agent Management**: Create, close, rename, and reorder agent terminals
- **Workspace Selection**: Native folder picker with git repository validation
- **Persistent Config**: Agent counter stored in `.loom/config.json` per workspace
- **Monotonic Numbering**: Agents always increment, persists across app restarts
- **Drag & Drop**: Reorder agent terminals in the mini row
- **Inline Renaming**: Double-click agent names to rename (doesn't affect numbering)
- **Tilde Expansion**: Support for `~/path` notation in workspace paths
- **Workspace-First**: Must select workspace before creating agents

## Technology Stack

### Frontend
- **Tauri 1.8.1**: Desktop app framework (Rust backend, web frontend)
- **TypeScript 5.9**: Strict mode enabled for maximum type safety
- **Vite 5**: Fast build tool with hot module replacement
- **TailwindCSS 3.4**: Utility-first CSS with dark mode support
- **Vanilla TS**: No framework overhead, direct DOM manipulation

### Backend
- **Rust**: Tauri backend with IPC commands
  - `validate_git_repo`: Validates git repository paths
  - `list_role_files`: Lists available role files from `.loom/roles/` and `defaults/roles/`
  - `read_role_file`: Reads role definition markdown files
  - `read_role_metadata`: Reads optional JSON metadata for roles
  - `greet`: Example command (will be removed)
- **Tauri APIs**: Dialog (file picker), Path (tilde expansion), Filesystem (role files)
- **Node.js**: For terminal process management (future)
- **Anthropic Claude**: AI agent integration (future)

### Why Vanilla TypeScript?

We deliberately chose vanilla TS over React/Vue/Svelte for:
1. **Performance**: Direct DOM manipulation, no virtual DOM overhead
2. **Learning**: Perfect for understanding fundamentals
3. **Simplicity**: No build complexity, no framework lock-in
4. **Control**: Full control over rendering and updates

## Project Structure

```
loom/
├── src/
│   ├── main.ts                      # Entry point, state init, events, workspace logic
│   ├── style.css                    # Global styles, Tailwind imports
│   └── lib/
│       ├── state.ts                 # State management (agents, workspace, observer)
│       ├── config.ts                # Config file I/O (.loom/config.json)
│       ├── ui.ts                    # UI rendering (pure functions)
│       ├── theme.ts                 # Dark/light theme system
│       └── terminal-settings-modal.ts # Terminal configuration modal
├── src-tauri/
│   ├── src/main.rs          # Rust backend, Tauri IPC commands
│   ├── tauri.conf.json      # Window config, allowlist, build settings
│   └── Cargo.toml           # Rust dependencies (tauri features)
├── .loom/                   # Workspace config (gitignored, per-workspace)
│   ├── config.json          # Persistent config (agent counter, roles, etc.)
│   └── roles/               # Custom role definitions (optional)
│       ├── my-role.md       # Role definition markdown
│       └── my-role.json     # Role metadata (optional)
├── defaults/                # Default configuration files (committed to git)
│   ├── config.json          # Default configuration template
│   └── roles/               # System role templates
│       ├── default.md       # Plain shell environment
│       ├── worker.md        # General development worker
│       ├── issues.md        # GitHub issue creation specialist
│       ├── reviewer.md      # Code review specialist
│       ├── architect.md     # System architecture and design
│       └── curator.md       # Issue maintenance and enhancement
├── index.html               # HTML structure (header, primary, mini row)
├── tsconfig.json            # TypeScript strict mode config
├── tailwind.config.js       # Tailwind with dark mode: 'class'
├── vite.config.ts           # Vite config for Tauri
└── package.json             # Dependencies, scripts (uses pnpm)
```

## Architecture Patterns

### 1. Observer Pattern (State Management)

**File**: `src/lib/state.ts`

```typescript
export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private listeners: Set<() => void> = new Set();

  // Notify all listeners when state changes
  private notify(): void {
    this.listeners.forEach(cb => cb());
  }

  // Subscribe to state changes
  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }
}
```

**Why Observer Pattern?**
- Decouples state from UI
- Single source of truth
- Automatic UI updates on state changes
- Easy to add new listeners (e.g., persist to localStorage)

**Key Features**:
- Map-based storage for O(1) agent terminal lookups
- Strong typing with `Terminal` interface and `TerminalStatus` enum
- Safety: Cannot remove last agent terminal
- Auto-promotion: First terminal becomes primary when current removed
- Workspace state: Separate valid workspace vs displayed path for error handling
- Monotonic agent numbering: Counter always increments, never reuses deleted numbers

### 2. Pure Functions (UI Rendering)

**File**: `src/lib/ui.ts`

All rendering functions are pure - same input always produces same output:

```typescript
export function renderPrimaryTerminal(terminal: Terminal | null): void {
  const container = document.getElementById('primary-terminal');
  if (!container) return;

  // Pure transformation: terminal data → HTML string
  container.innerHTML = createPrimaryTerminalHTML(terminal);
}
```

**Why Pure Functions?**
- Predictable and testable
- No hidden side effects
- Easy to reason about
- Can be memoized later for performance

**XSS Protection**: All user input goes through `escapeHtml()` before rendering

### 3. Event Delegation

**File**: `src/main.ts`

Instead of adding listeners to each terminal card, we use delegation:

```typescript
// One listener on parent handles all mini terminal clicks
document.getElementById('mini-terminal-row')?.addEventListener('click', (e) => {
  const target = e.target as HTMLElement;
  const card = target.closest('[data-terminal-id]');

  if (card && !target.classList.contains('close-terminal-btn')) {
    const id = card.getAttribute('data-terminal-id');
    if (id) state.setPrimary(id);
  }
});
```

**Why Event Delegation?**
- Better performance (fewer listeners)
- Works with dynamically added elements
- Simpler cleanup (no need to remove individual listeners)

### 4. Reactive Rendering

The render cycle:

```
State Change → notify() → onChange callbacks → render() → setupEventListeners()
```

**Important**: `setupEventListeners()` is called after every render to re-attach handlers to new DOM elements. This is intentional and works because:
1. Old elements are removed (garbage collected)
2. New elements need fresh event listeners
3. Event delegation minimizes performance impact

### 5. Tauri IPC (Inter-Process Communication)

**Files**: `src/main.ts`, `src-tauri/src/main.rs`

Tauri provides a bridge between TypeScript frontend and Rust backend:

**Frontend** (TypeScript):
```typescript
import { invoke } from '@tauri-apps/api/tauri';

const isValid = await invoke<boolean>('validate_git_repo', { path });
```

**Backend** (Rust):
```rust
#[tauri::command]
fn validate_git_repo(path: String) -> Result<bool, String> {
    // Validation logic with full filesystem access
}
```

**Why Use Rust Commands?**
- Bypass client-side filesystem restrictions
- Full native filesystem access
- Type-safe IPC with automatic serialization
- Better error handling and security

**Current Commands**:
- `validate_git_repo(path: String)`: Validates path is a git repository
  - Checks path exists and is a directory
  - Verifies `.git` directory exists
  - Returns `Result<bool, String>` with specific error messages
- `reset_github_labels()`: Resets GitHub label state machine during workspace restart
  - Removes `loom:in-progress` from all open issues
  - Replaces `loom:reviewing` with `loom:review-requested` on all open PRs
  - Returns `LabelResetResult` with counts and errors
  - Called automatically during both start-workspace and force-start-workspace
  - Non-critical operation - continues on error

**Workspace Validation Pattern**:
```typescript
// Separate state: displayedWorkspacePath (shown) vs workspacePath (valid)
state.setDisplayedWorkspace(userInput);  // Show immediately
const isValid = await validateWorkspacePath(userInput);
if (isValid) {
  state.setWorkspace(userInput);  // Mark as valid
} else {
  state.setWorkspace('');  // Keep displayed but don't use
}
```

This allows showing invalid paths with error messages while preventing use of invalid workspace.

### 6. Persistent Configuration

**Files**: `src/lib/config.ts`, `.loom/config.json`

Loom stores workspace-specific configuration in `.loom/config.json` within each git repository:

```json
{
  "nextAgentNumber": 4,
  "agents": [
    {
      "id": "1",
      "name": "Shell",
      "status": "idle",
      "isPrimary": true
    },
    {
      "id": "2",
      "name": "Worker 1",
      "status": "idle",
      "isPrimary": false,
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "worker.md",
        "targetInterval": 300000,
        "intervalPrompt": "Continue working on open tasks"
      }
    }
  ]
}
```

**Why Workspace-Specific Config?**
- Each git repo has independent agent numbering and terminal configurations
- Config persists across app restarts
- No parsing of agent names (users can rename freely)
- Stored in workspace, not in app directory
- Role assignments and autonomous settings preserved

**Config Lifecycle**:
```typescript
// 1. User selects workspace
await handleWorkspacePathInput('/path/to/repo');

// 2. Set config workspace path
setConfigWorkspace('/path/to/repo');

// 3. Load config from .loom/config.json
const config = await loadConfig();  // { nextAgentNumber: 1, agents: [...] } or existing

// 4. Initialize state
state.setNextAgentNumber(config.nextAgentNumber);
state.restoreAgents(config.agents);

// 5. User creates agent
const num = state.getNextAgentNumber();  // Returns 1, increments to 2
state.addTerminal({ name: `Agent ${num}`, ... });

// 6. User configures terminal role via settings modal
state.updateTerminalRole(id, 'claude-code-worker', {
  workerType: 'claude',
  roleFile: 'worker.md',
  targetInterval: 300000,
  intervalPrompt: 'Continue working on open tasks'
});

// 7. Save updated config
await saveConfig({
  nextAgentNumber: state.getCurrentAgentNumber(),
  agents: state.getTerminals()
});
```

**File Operations**:
- Uses Tauri fs API (`readTextFile`, `writeTextFile`, `exists`, `createDir`)
- Creates `.loom/` directory if it doesn't exist
- Falls back to defaults if config file missing
- Gracefully handles read/write errors

**Important**: `.loom/` is gitignored - each developer has their own agent numbering and terminal configurations.

### 7. Terminal Configuration System

**Files**: `src/lib/terminal-settings-modal.ts`, `src-tauri/src/main.rs` (role file commands)

The terminal configuration system allows users to assign specialized roles to each terminal through a settings modal.

**Role Definition Structure**:

Each role consists of two files:
- **`.md` file** (required): The role definition text with markdown formatting
- **`.json` file** (optional): Metadata with default settings

**Role Metadata Schema**:
```json
{
  "name": "Worker Bot",
  "description": "General development worker for features, bugs, and refactoring",
  "defaultInterval": 0,
  "defaultIntervalPrompt": "Continue working on open tasks",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
```

**Role File Resolution**:
1. Check workspace-specific: `.loom/roles/<filename>`
2. Fall back to defaults: `defaults/roles/<filename>`
3. List command merges both, workspace files take precedence

**Available Roles** (from `defaults/roles/`):

| Role | File | Autonomous | Interval | Description |
|------|------|-----------|----------|-------------|
| **Default** | `default.md` | No | N/A | Plain shell environment, no specialized role |
| **Worker** | `worker.md` | No | 0 (manual) | General development worker for features, bugs, and refactoring |
| **Issues** | `issues.md` | No | 0 (manual) | Specialist for creating well-structured GitHub issues |
| **Reviewer** | `reviewer.md` | Yes | 5 min | Code review specialist for thorough PR reviews |
| **Architect** | `architect.md` | Yes | 15 min | System architecture and technical decision making |
| **Curator** | `curator.md` | Yes | 5 min | Issue maintenance and quality improvement |

**Autonomous Mode**:
- When `targetInterval > 0`, the terminal will automatically execute the `intervalPrompt` at regular intervals
- Example: Reviewer bot runs every 5 minutes with prompt "Find and review open PRs with loom:review-requested label"
- Allows terminals to work autonomously without user intervention
- Recommended for Curator, Reviewer, and Architect roles

**Label-based Workflow Coordination**:

Roles coordinate work through GitHub labels (see [WORKFLOWS.md](WORKFLOWS.md) for complete details):

1. **Architect** creates issues with `loom:architect-suggestion` label
2. User reviews and removes label to approve
3. **Curator** finds unlabeled issues, enhances them, marks as `loom:ready`
4. **Worker** claims `loom:ready` issues, implements, creates PR with `loom:review-requested`
5. **Reviewer** finds `loom:review-requested` PRs, reviews, approves/requests changes
6. User merges approved PRs

**Terminal Settings Modal UI**:

The modal provides:
- Role file dropdown (populated from both workspace and default roles)
- Worker type selection (Claude or Codex)
- Autonomous mode checkbox
- Interval configuration (milliseconds)
- Interval prompt textarea
- Save/Cancel buttons

**Implementation Pattern**:
```typescript
// 1. User clicks settings icon on terminal card
openTerminalSettings(terminalId);

// 2. Modal loads available role files via Tauri command
const roleFiles = await invoke<string[]>('list_role_files', { workspacePath });

// 3. User selects role file, modal loads metadata if available
const metadata = await invoke<string | null>('read_role_metadata', {
  workspacePath,
  filename: selectedFile
});

// 4. Form pre-populates with metadata defaults or current config
populateFormFromMetadata(metadata);

// 5. User configures settings and saves
state.updateTerminalRole(terminalId, role, roleConfig);
await saveConfig({ /* ... */ });
```

**Custom Roles**:

Users can create custom roles by adding files to `.loom/roles/` in their workspace:

```markdown
<!-- .loom/roles/my-custom-role.md -->
# My Custom Role

You are a specialist in the {{workspace}} repository.

## Your Role
...
```

```json
// .loom/roles/my-custom-role.json
{
  "name": "My Custom Role",
  "description": "Brief description",
  "defaultInterval": 600000,
  "defaultIntervalPrompt": "The prompt to send at each interval",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
```

Template variables:
- `{{workspace}}`: Replaced with absolute path to workspace directory

See [defaults/roles/README.md](defaults/roles/README.md) for detailed guidance on creating custom roles.

### 8. Git Worktrees and Sandbox Compatibility

**Files**: `src/lib/worktree-manager.ts`, `src/lib/agent-launcher.ts`, `loom-daemon/src/terminal.rs`

Loom uses git worktrees to provide isolated working directories for each agent terminal. This allows multiple agents to work on different features simultaneously without conflicts.

**Worktree Path Configuration**:

All agent worktrees are created inside the workspace at:
```
${workspacePath}/.loom/worktrees/${terminalId}
```

This design is **sandbox-compatible** because:
- Worktrees stay inside the workspace directory (no external paths)
- Already gitignored via `.gitignore` line 34: `.loom/worktrees/`
- Each terminal gets its own isolated working directory
- No shared state or conflicts between agents

**On-Demand Worktree Creation** (`scripts/worktree.sh`):

Agents create worktrees when claiming issues using the helper script:

```bash
# Agent claims issue and creates worktree
./.loom/scripts/worktree.sh 42

# This runs the helper script which:
# 1. Validates issue number
# 2. Checks for nested worktrees (prevents if already in one)
# 3. Creates worktree at .loom/worktrees/issue-42
# 4. Creates branch feature/issue-42 from main
# 5. Provides clear instructions for next steps
```

**Manual Worktree Creation** (`src/lib/worktree-manager.ts:28`):

The old `setupWorktreeForAgent()` function still exists but is no longer called automatically during workspace start. It can be used programmatically if needed.

**Daemon Auto-Cleanup** (`loom-daemon/src/terminal.rs:87-102`):

When a terminal is destroyed, the daemon automatically detects and removes worktrees:

```rust
// Check if working directory is a Loom worktree
if working_directory.contains("/.loom/worktrees/") {
    // Remove from git worktrees
    Command::new("git")
        .arg("worktree")
        .arg("remove")
        .arg(&working_directory)
        .arg("--force")
        .output()
        .ok();
}
```

**IMPORTANT: Understanding Worktree Contexts**:

There are **two completely different contexts** for worktrees in Loom, and this is critical to understand:

### Context 1: Agents Running Inside Loom (Normal Use)

**Agents start in the main workspace, not in worktrees.** Worktrees are created on-demand when claiming issues:

- Agents begin in the main workspace directory (not isolated)
- To work on an issue: `./.loom/scripts/worktree.sh <issue-number>` creates `.loom/worktrees/issue-{number}`
- Helper script prevents nested worktrees and ensures proper paths
- Multiple agents can work simultaneously by each claiming their own issue
- Worktrees are named semantically by issue number, not terminal ID

**For agents**: Use `./.loom/scripts/worktree.sh <issue>` when claiming an issue. Create feature branches in your worktree.

### Context 2: Human Developers Working on Loom's Codebase (Dogfooding)

When **human developers** (not agents) want to work on Loom issues manually outside the app, use the worktree helper script:

```bash
# ✅ CORRECT - Use the helper script
./.loom/scripts/worktree.sh 84

# ✅ With custom branch name
./.loom/scripts/worktree.sh 84 my-custom-branch

# ✅ Check if you're already in a worktree
./.loom/scripts/worktree.sh --check

# ❌ WRONG - Don't run git worktree commands directly
git worktree add .loom/worktrees/issue-84 -b feature/issue-84 main
```

**Why Use the Helper Script?**

1. **Prevents Nested Worktrees**: Automatically detects if you're already in a worktree and prevents accidental nesting
2. **Consistent Paths**: Always creates worktrees at `.loom/worktrees/issue-{number}` (sandbox-safe)
3. **Automatic Branch Naming**: Prefixes branches with `feature/` automatically
4. **Error Prevention**: Clear error messages instead of cryptic git errors
5. **Safety Checks**: Validates issue numbers, checks for existing directories, handles existing branches

**Worktree Helper Usage**:

```bash
# Basic usage - creates worktree for issue #42
./.loom/scripts/worktree.sh 42
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/issue-42

# Custom branch name
./.loom/scripts/worktree.sh 42 fix-critical-bug
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/fix-critical-bug

# Check current worktree status
./.loom/scripts/worktree.sh --check
# → Shows: Current worktree path and branch (or confirms you're in main)

# Show help
./.loom/scripts/worktree.sh --help
```

**When to Use the Helper Script**:

- **Human developers** working on Loom codebase issues manually
- **NOT for agents** (they already have worktrees)
- **NOT needed when using Loom itself** (it creates worktrees automatically)

**Human Developer Workflow**:

```bash
# 1. Starting work on issue #123 (from main workspace)
cd /Users/rwalters/GitHub/loom
./.loom/scripts/worktree.sh 123
cd .loom/worktrees/issue-123
# Work on the issue, commit, push, create PR

# 2. Check if you're in a worktree
./.loom/scripts/worktree.sh --check

# 3. Returning to main after finishing
cd /Users/rwalters/GitHub/loom
git checkout main
```

**Error Handling**:

The helper script provides clear guidance for common issues:

- **Already in a worktree**: Shows current worktree info and instructions to return to main
- **Directory exists**: Checks if it's a valid worktree or needs cleanup
- **Branch exists**: Prompts whether to use existing branch or create new one
- **Invalid issue number**: Rejects non-numeric input with usage help

**Implementation**: See `scripts/worktree.sh` for the full implementation

**Workflow for Agents**:

When agents running inside Loom work on issues:

```bash
# 1. Claim issue and create worktree
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:in-progress"
./.loom/scripts/worktree.sh 42
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/issue-42

# 2. Change to worktree
cd .loom/worktrees/issue-42

# 3. Do the work
# ... implement, test, commit ...

# 4. Push and create PR
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"

# 5. Return to main workspace
cd ../..
```

**Benefits of On-Demand Worktree System**:

1. **Semantic Naming**: Worktrees named by issue number (`.loom/worktrees/issue-42`), not terminal ID
2. **On-Demand Creation**: Only create worktrees when needed, reducing resource usage
3. **No Nested Worktrees**: Helper script prevents accidental nesting and provides clear error messages
4. **Isolation When Needed**: Each agent can work on separate issues without conflicts
5. **Clean Workspace**: Agents start in main workspace, create worktrees only for implementation
6. **Gitignored**: Worktrees don't clutter git status
7. **Sandbox-Safe**: All worktrees inside workspace, no filesystem escapes

**TypeScript Worktree Setup** (`src/lib/agent-launcher.ts:27-34`):

```typescript
let agentWorkingDir = workspacePath;
if (useWorktree && !worktreePath) {
  const { setupWorktreeForAgent } = await import("./worktree-manager");
  agentWorkingDir = await setupWorktreeForAgent(terminalId, workspacePath, gitIdentity);
}
```

**Testing**: See `src/lib/worktree-manager.test.ts` for comprehensive test coverage including:
- Directory structure creation
- Git worktree creation from HEAD
- Git identity configuration
- Command execution ordering
- Path handling with spaces and special characters
- Terminal input simulation

## TypeScript Conventions

### Strict Mode

`tsconfig.json` has strict mode enabled:
- `strict: true` - All strict checks
- `noUnusedLocals: true` - No unused variables
- `noUnusedParameters: true` - No unused function parameters
- `noFallthroughCasesInSwitch: true` - Explicit breaks in switch

### Type Safety Patterns

1. **Enums for fixed sets**:
   ```typescript
   export enum TerminalStatus {
     Idle = 'idle',
     Busy = 'busy',
     NeedsInput = 'needs_input',
     Error = 'error',
     Stopped = 'stopped'
   }
   ```

2. **Interfaces for data structures**:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
   }
   ```

3. **Return types for cleanup**:
   ```typescript
   onChange(callback: () => void): () => void {
     this.listeners.add(callback);
     return () => this.listeners.delete(callback); // Cleanup function
   }
   ```

## Code Quality Tools (Issue #8)

### Linting & Formatting Setup

**Frontend (Biome)**:
- Fast, comprehensive linter and formatter for TypeScript/JavaScript
- Configured in `biome.json` with schema version 2.2.5
- VCS integration enabled for git-aware linting
- Rules: Recommended + custom overrides for project style
- Commands: `npm run lint`, `npm run format`

**Backend (rustfmt + clippy)**:
- `rustfmt.toml`: Format configuration (100 char width, 4 space indent)
- `.cargo/config.toml`: Clippy lint levels
  - Deny: all, correctness, suspicious, complexity
  - Warn: pedantic, perf, style, unwrap_used, expect_used
- Commands: `npm run format:rust`, `npm run clippy`

**Git Hooks (husky + lint-staged)**:
- Pre-commit hook auto-formats staged files
- TS/JS files: Biome formatting + linting
- Rust files: rustfmt formatting
- Configured in `.husky/pre-commit` and `package.json`

**CI/CD (GitHub Actions)**:
- Workflow: `.github/workflows/ci.yml`
- Jobs run in parallel: frontend lint/format, rust format, rust clippy, builds
- All warnings treated as errors (`-D warnings` for clippy)
- Dependency caching for faster builds
- Frontend build artifacts downloaded before Tauri compilation

**VSCode Integration**:
- Settings: `.vscode/settings.json`
- Extensions: `.vscode/extensions.json`
- Format on save enabled for all languages
- Biome for TS/JS, rust-analyzer for Rust

### Development Workflow

1. **Make changes** - Edit code with format-on-save
2. **Pre-commit hook** - Auto-formats on commit
3. **Push** - Triggers CI checks
4. **CI validates** - All linting/formatting/builds must pass
5. **Manual check** - Run `npm run check:all` to verify locally

### IMPORTANT: Always Use pnpm Scripts for CI Matching

**Always use pnpm scripts** defined in `package.json` instead of running cargo/biome commands directly. This ensures your local checks match CI exactly.

**Available Scripts**:
```bash
pnpm lint              # Biome linting
pnpm format            # Biome formatting
pnpm format:rust       # Rust formatting check
pnpm format:rust:write # Rust formatting fix
pnpm clippy            # Clippy with exact CI flags
pnpm clippy:fix        # Clippy auto-fix
pnpm check             # Cargo check
pnpm build             # TypeScript + Vite build
pnpm check:all         # Run everything (full CI simulation)
```

**Why This Matters**:
- CI uses: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- Direct `cargo clippy` might miss flags like `--all-targets` or `--all-features`
- pnpm scripts guarantee the exact same command CI runs
- Prevents "passes locally but fails in CI" issues

**Before Opening a PR**:
```bash
pnpm check:all  # This runs the full CI suite locally
```

If this passes, CI should pass too.

**Package Manager Preference**: Always use `pnpm` (not `npm`) as the package manager for this project.

**Development Workflow**:

Use the appropriate script based on your scenario:

- **`pnpm app:dev`**: Normal development with hot reload (fastest iteration)
  - Use when: Making frequent frontend changes
  - Caveat: Hot reload sometimes misses changes (see "Stale Code Issue" below)

- **`pnpm app:preview`**: Complete rebuild + launch (recommended for testing)
  - Use when: After pulling new code, switching branches, or hot reload misses changes
  - Always rebuilds both frontend AND Tauri binary before launching
  - This is the "safe" option that guarantees fresh code

- **`pnpm app:build`**: Production build
  - Use when: Creating release builds

**Stale Code Issue**: If you pull new code or switch branches, run `pnpm app:preview` to ensure you're running the latest code. The `tauri dev` command caches the built frontend and hot reload doesn't always catch everything, leading to wasted debugging time.

### Clippy Configuration Details

The `.cargo/config.toml` enforces strict linting:

```toml
rustflags = [
    "-D", "clippy::all",           # Deny all warnings
    "-D", "clippy::correctness",   # Deny correctness issues
    "-D", "clippy::suspicious",    # Deny suspicious patterns
    "-D", "clippy::complexity",    # Deny unnecessary complexity
    "-W", "clippy::pedantic",      # Warn on pedantic issues
    "-W", "clippy::unwrap_used",   # Warn on .unwrap()
    "-W", "clippy::expect_used",   # Warn on .expect()
]
```

**When to use `#[allow(clippy::expect_used)]`**:
- Mutex locks (poisoning is panic-level, not recoverable)
- Main function startup (Tauri failure is fatal)
- Other truly exceptional scenarios

**Handling expect/unwrap warnings**:
- Prefer proper error handling with `Result` and `?` operator
- Use `expect()` with descriptive messages only when panic is acceptable
- Add `#[allow]` attribute with explanatory comment when necessary

## Self-Modification Problem

**CRITICAL**: Loom cannot develop itself using `app:dev` mode due to hot reload causing restart loops.

### The Problem

When running Loom in development mode (`pnpm app:dev`), Vite watches for file changes and triggers hot module replacement (HMR). If agent terminals within Loom are working on the Loom codebase itself:

1. Agent edits source file (e.g., `src/lib/workspace-reset.ts`)
2. Vite detects change and triggers HMR
3. Tauri reloads the app
4. App restart interrupts the agent mid-work
5. Agent continues, edits another file
6. **Infinite restart loop**

This makes it impossible for Loom to work on its own codebase in dev mode.

### Solutions

**Option 1: Use Preview Mode (Recommended)**
```bash
pnpm app:preview
```
- Builds the app once, then runs without hot reload
- Agents can edit files without triggering restarts
- Still faster than full production builds
- Requires rebuild to see UI changes

**Option 2: Use Production Mode**
```bash
pnpm app:build
# Then run the built app from ./target/release/
```
- Fully optimized production build
- No hot reload at all
- Slowest rebuild cycle

**Option 3: Work on Different Workspace**
```bash
# Clone Loom to a separate directory
git clone https://github.com/your-username/loom ~/loom-dev
cd ~/loom-dev
pnpm app:preview

# Point agent terminals at original workspace
# Agents work in ~/GitHub/loom, app runs from ~/loom-dev
```
- Separates running app from workspace being edited
- Best for testing factory reset and agent features
- Requires keeping both repos in sync

**Option 4: Disable Agent Terminals**
```bash
# Use app:dev but don't run any agent terminals
pnpm app:dev
# Keep terminals as plain shells or run agents on different repos
```
- Good for frontend development only
- Can't test agent orchestration features

### When to Use Each Mode

**Use `app:dev`**:
- Frontend-only development (CSS, UI components, layouts)
- No agent terminals running
- Working on a different repository with agent terminals

**Use `app:preview`**:
- Testing factory reset, agent launching, worktree management
- Agent terminals working on Loom codebase
- Integration testing with agents

**Use `app:build`**:
- Final testing before release
- Performance profiling
- Packaging for distribution

### Vite Ignore Configuration

Vite is already configured to ignore `.loom/**` directories:

```typescript
// vite.config.ts
export default defineConfig({
  server: {
    watch: {
      ignored: ["**/.loom/**"],  // Ignores .loom/worktrees/, .loom/config.json, etc.
    },
  },
});
```

However, this only prevents changes to `.loom/` from triggering reloads. Changes to `src/**`, `loom-daemon/**`, or other source files will still trigger HMR.

### Summary

**DO NOT use `app:dev` when agent terminals are working on the Loom codebase.** Always use `app:preview` or `app:build` for self-modification scenarios.

## Styling Conventions

### TailwindCSS Usage

1. **Utility-first**: Use Tailwind classes directly in HTML/JS
2. **Dark mode**: All colors have `dark:` variants
3. **Transitions**: Global 300ms transitions in `style.css`
4. **Semantic colors**: Status indicators use semantic mapping

```typescript
function getStatusColor(status: TerminalStatus): string {
  return {
    [TerminalStatus.Idle]: 'bg-green-500',
    [TerminalStatus.Busy]: 'bg-blue-500',
    [TerminalStatus.NeedsInput]: 'bg-yellow-500',
    [TerminalStatus.Error]: 'bg-red-500',
    [TerminalStatus.Stopped]: 'bg-gray-400'
  }[status];
}
```

### Theme System

**File**: `src/lib/theme.ts`

- Dark mode via `class="dark"` on `<html>`
- Persists to localStorage
- Respects system preference on first load
- Instant color changes (no transitions for better UX)

```typescript
export function toggleTheme(): void {
  const isDark = document.documentElement.classList.toggle('dark');
  localStorage.setItem('theme', isDark ? 'dark' : 'light');
}
```

**Design Choice**: Theme transitions were intentionally removed because animated color changes during theme toggle were distracting and made the interface feel sluggish.

### Custom CSS

Minimal custom CSS in `src/style.css`:
- Tailwind imports
- Custom scrollbars (webkit)
- Smooth scrolling for mini terminal row
- Drop indicator for drag-and-drop
- User-select: none on draggable cards

## State Flow Diagram

```
┌─────────────┐
│   AppState  │  (Single source of truth)
└──────┬──────┘
       │
       │ addTerminal()
       │ removeTerminal()
       │ setPrimary()
       │
       ↓
   ┌───────┐
   │notify()│
   └───┬───┘
       │
       ↓
┌──────────────┐
│  onChange    │  (Multiple listeners)
│  callbacks   │
└──────┬───────┘
       │
       ↓
   ┌────────┐
   │render()│  (Re-render entire UI)
   └────┬───┘
        │
        ├──→ renderHeader()
        ├──→ renderPrimaryTerminal()
        └──→ renderMiniTerminals()
             └──→ setupEventListeners()
```

## Common Tasks

### Adding a New Agent Terminal Property

1. Update interface in `src/lib/state.ts`:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
     workingDirectory?: string; // NEW
   }
   ```

2. Update UI rendering in `src/lib/ui.ts`:
   ```typescript
   // Display new property
   <span>${escapeHtml(terminal.workingDirectory || 'N/A')}</span>
   ```

3. TypeScript will catch any missing properties at compile time

### Adding a New State Method

1. Add method to `AppState` class in `src/lib/state.ts`
2. Call `this.notify()` after state changes
3. UI will automatically re-render

### Adding a New UI Section

1. Add HTML structure to `index.html`
2. Create render function in `src/lib/ui.ts`
3. Call from `render()` in `src/main.ts`
4. Add event listeners in `setupEventListeners()`

### Adding a New Tauri Command

1. **Add Rust command** in `src-tauri/src/main.rs`:
   ```rust
   #[tauri::command]
   fn my_command(param: String) -> Result<ReturnType, String> {
       // Implementation
       Ok(result)
   }
   ```

2. **Register command** in `main()`:
   ```rust
   tauri::Builder::default()
       .invoke_handler(tauri::generate_handler![my_command])
   ```

3. **Call from TypeScript** in `src/main.ts`:
   ```typescript
   import { invoke } from '@tauri-apps/api/tauri';

   const result = await invoke<ReturnType>('my_command', { param: value });
   ```

4. **Add required APIs** to `src-tauri/tauri.conf.json` allowlist if needed
5. **Update Cargo.toml** if new Tauri features required

### Debugging

1. **State inspection**: Add `console.log(state.getTerminals())` in render
2. **TypeScript errors**: Run `pnpm exec tsc --noEmit`
3. **Hot reload**: Vite provides instant feedback on save
4. **Tauri DevTools**: Open with Cmd+Option+I in dev mode

## Testing Strategy

### Daemon Integration Tests (Issue #13)

**Location**: `loom-daemon/tests/`

Comprehensive integration test suite for the daemon with 9 passing test cases:

**Test Infrastructure** (`tests/common/mod.rs`):
- `TestDaemon`: Manages isolated daemon instances with unique socket paths
- `TestClient`: Async IPC client with helper methods for all operations
- tmux helper functions for session management and cleanup
- Proper isolation with `#[serial]` attribute to prevent race conditions

**Test Coverage** (`tests/integration_basic.rs`):
1. Basic IPC (Ping/Pong, malformed JSON handling)
2. Terminal lifecycle (create, list, destroy)
3. Working directory support
4. Input handling
5. Multiple concurrent clients
6. Error conditions (non-existent terminals)

**Running Tests**:
```bash
npm run daemon:test                    # Run all daemon tests
npm run daemon:test:verbose           # With full output
cargo test --test integration_basic   # Run specific test file
```

**Key Implementation Details**:
- Daemon uses internally-tagged JSON: `{"type": "Ping"}`, `{"type": "CreateTerminal", "payload": {...}}`
- Tests use `LOOM_SOCKET_PATH` env var for isolation
- Each test spawns isolated daemon in temp directory
- Automatic cleanup on test completion

### Frontend Testing (Planned)

1. **Unit Tests**: Vitest for pure functions (state.ts, ui.ts)
2. **Integration Tests**: Playwright for E2E workflows
3. **Type Tests**: TypeScript strict mode as first line of defense

## MCP Testing and Instrumentation

**Location**: `mcp-loom-ui/`, `mcp-loom-logs/`, `mcp-loom-terminals/`, `.mcp.json`

Loom provides three MCP (Model Context Protocol) servers that enable AI agents (including Claude Code) to inspect and interact with the running app for testing and debugging.

**📖 Full API Documentation**: [docs/mcp/README.md](docs/mcp/README.md)

**Available Servers**:
- **[mcp-loom-ui](docs/mcp/loom-ui.md)** - UI interaction, console logs, workspace state (7 tools)
- **[mcp-loom-logs](docs/mcp/loom-logs.md)** - Daemon, Tauri, and terminal logs (4 tools)
- **[mcp-loom-terminals](docs/mcp/loom-terminals.md)** - Terminal management and IPC (4 tools)

### Console Logging to File

**Implementation**: `src/main.ts` (console interceptor) + `src-tauri/src/main.rs` (`append_to_console_log`)

All browser console output is automatically written to `~/.loom/console.log`:

```typescript
// Console interception (src/main.ts)
const originalConsoleLog = console.log;
console.log = (...args: unknown[]) => {
  originalConsoleLog(...args);  // Still log to DevTools
  writeToConsoleLog("INFO", ...args);  // Also write to file
};
```

**Log Format**:
```
[2025-10-15T05:05:06.088Z] [INFO] [launchAgentsForTerminals] Starting agent launch...
[2025-10-15T05:05:06.814Z] [INFO] [launchAgentInTerminal] Worktree setup complete
```

**Benefits**:
- Persistent logs survive app restarts
- AI agents can read logs via MCP to diagnose issues
- Debug output visible without watching DevTools in real-time
- Full visibility into factory reset and agent launch processes

### MCP Loom UI Server

**Package**: `mcp-loom-ui/`
**Configuration**: `.mcp.json`

MCP server providing tools for Claude Code to interact with Loom's state and logs:

**Available Tools**:

1. **`read_console_log`**
   - Reads browser console output from `~/.loom/console.log`
   - Returns recent log entries with timestamps
   - Use for debugging workspace start, agent launch, worktree setup

2. **`read_state_file`**
   - Reads current application state from `.loom/state.json`
   - Shows active terminals, session IDs, working directories
   - Use for verifying terminal creation and state management

3. **`read_config_file`**
   - Reads terminal configurations from `.loom/config.json`
   - Shows terminal roles, intervals, prompts
   - Use for verifying configuration persistence

4. **`trigger_start`**
   - Start engine with EXISTING config (shows confirmation dialog)
   - Uses current `.loom/config.json` to create terminals and launch agents
   - Does NOT reset or overwrite configuration
   - Use for restarting terminals after app restart or crash

5. **`trigger_force_start`**
   - Start engine with existing config WITHOUT confirmation
   - Same as trigger_start but bypasses confirmation prompt
   - Use for MCP automation and testing

6. **`trigger_factory_reset`**
   - Reset workspace to factory defaults (shows confirmation dialog)
   - Overwrites `.loom/config.json` with `defaults/config.json`
   - Does NOT auto-start the engine - must run trigger_start/force_start after
   - Use for resetting configuration to clean state

**MCP Configuration** (`.mcp.json`):
```json
{
  "mcpServers": {
    "loom-ui": {
      "command": "node",
      "args": ["mcp-loom-ui/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/Users/rwalters/GitHub/loom"
      }
    }
  }
}
```

**Usage Example** (from Claude Code):
```bash
# Read recent console logs to see workspace start progress
mcp__loom-ui__read_console_log

# Check terminal state after start
mcp__loom-ui__read_state_file

# Check terminal configuration
mcp__loom-ui__read_config_file

# Start engine with existing config (bypasses confirmation for MCP automation)
mcp__loom-ui__trigger_force_start

# Reset workspace to defaults (requires separate start command after)
mcp__loom-ui__trigger_factory_reset
```

### Testing Workspace Start with MCP

**Goal**: Verify workspace start creates 7 terminals with Claude Code agents running autonomously in the main workspace

**Test Procedure**:

1. **Start Engine** (use force_start for MCP automation):
   ```bash
   mcp__loom-ui__trigger_force_start
   ```

2. **Monitor Console Logs**:
   ```bash
   mcp__loom-ui__read_console_log
   ```
   Look for:
   - `[start-workspace] Killing all loom tmux sessions`
   - `[start-workspace] ✓ Created terminal X`
   - `[launchAgentInTerminal] ✓ Agent will start in main workspace`
   - `[launchAgentInTerminal] Sending "2" to accept warning`

3. **Verify State**:
   ```bash
   mcp__loom-ui__read_state_file
   ```
   Confirm 7 terminals exist with correct session IDs (no worktree paths yet)

4. **Verify Main Workspace** (agents start here, create worktrees on-demand):
   ```bash
   ls -la .loom/worktrees/
   # Should be empty or show only manually created worktrees
   # Agents will create .loom/worktrees/issue-{number} when claiming issues
   ```

**Expected Success Criteria**:
- ✅ 7 terminals created (terminal-1 through terminal-7)
- ✅ All terminals start in main workspace directory
- ✅ NO automatic worktrees created during startup
- ✅ Claude Code running in all 7 terminals (bypass permissions accepted)
- ✅ No "command not found" or "duplicate session" errors
- ✅ Console logs show successful agent launch sequence

**Note**: Agents now start in the main workspace and create worktrees on-demand using `./.loom/scripts/worktree.sh <issue>` when claiming GitHub issues. This prevents resource waste and provides semantic naming (`.loom/worktrees/issue-42` instead of `terminal-1`).

**Factory Reset + Start Workflow**:

To reset configuration AND start the engine:

```bash
# Step 1: Reset config to defaults (does NOT auto-start)
mcp__loom-ui__trigger_factory_reset

# Step 2: Start engine with reset config
mcp__loom-ui__trigger_force_start
```

### Debugging Common Issues

**Issue**: Commands concatenated in terminal output
- **Symptom**: `claude --dangerously-skip-permissions2` or multiple commands on one line
- **Check**: Console logs for timing of `send_terminal_input` calls
- **Fix**: Increase delay in `worktree-manager.ts` `sendCommand()` function

**Issue**: "duplicate session" errors
- **Symptom**: `fatal: duplicate session: loom-terminal-X`
- **Check**: tmux sessions before factory reset: `tmux -L loom list-sessions`
- **Fix**: `kill_all_loom_sessions` should run before creating terminals

**Issue**: Bypass permissions prompt not accepted
- **Symptom**: Terminals stuck at "WARNING: Claude Code running in Bypass Permissions mode"
- **Check**: Terminal output files for prompt appearance timing
- **Fix**: Adjust retry delays in `agent-launcher.ts`

**Issue**: Worktree creation fails
- **Symptom**: `fatal: '/path' already exists` or `is a missing but already registered worktree`
- **Check**: Existing worktrees: `git worktree list`
- **Fix**: Prune orphaned worktrees: `git worktree prune`

## Performance Considerations

### Current Optimizations

1. **Map-based state**: O(1) terminal lookups
2. **Event delegation**: Minimal listener count
3. **Pure functions**: Easy to optimize later with memoization
4. **No virtual DOM**: Direct DOM manipulation

### Future Optimizations

1. **Virtual scrolling**: For 100+ terminals in mini row
2. **Memoization**: Cache rendered HTML for unchanged terminals
3. **Web Workers**: Move state logic off main thread
4. **Incremental rendering**: Only update changed sections

## Security Considerations

### XSS Prevention

All user input is escaped before rendering:

```typescript
function escapeHtml(text: string): string {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
```

This prevents malicious terminal names from injecting HTML/JS.

### Future Security

- Tauri IPC will be used for process spawning (sandboxed)
- API keys stored in system keychain (not .env)
- GitHub OAuth for authentication

## Structured Logging System (Issue #130)

Loom implements a lightweight structured logging system with JSON-formatted log entries for easy parsing and debugging.

### Frontend Logging

**File**: `src/lib/logger.ts`

```typescript
import { Logger } from "./logger";

// Create logger for component
const logger = Logger.forComponent("worktree-manager");

// Log informational message with context
logger.info("Creating worktree", {
  terminalId: "terminal-1",
  worktreePath: "/path/to/worktree",
});

// Log error with full context
logger.error("Failed to create worktree", error, {
  terminalId: "terminal-1",
  worktreePath: "/path/to/worktree",
});
```

**Log Output Format**:
```json
{
  "timestamp": "2025-10-15T05:05:06.088Z",
  "level": "INFO",
  "message": "Creating worktree",
  "context": {
    "component": "worktree-manager",
    "terminalId": "terminal-1",
    "worktreePath": "/path/to/worktree"
  }
}
```

### Backend Logging

**File**: `loom-daemon/src/logging.rs`

```rust
use crate::{log_info, log_error};

// Log informational message
log_info!("Terminal created", {
    component: "terminal",
    terminal_id: Some(id.clone()),
    working_dir: path
});

// Log error
log_error!("Failed to spawn tmux", &err, {
    component: "terminal",
    terminal_id: Some(id.clone())
});
```

### Logging Conventions

**When to Use Each Log Level**:

1. **INFO**: Normal operations, milestones, state transitions
   - Component initialization
   - Successful operations (terminal created, worktree setup complete)
   - State changes (workspace loaded, agent launched)

2. **WARN**: Unexpected but recoverable conditions
   - Non-fatal errors (failed to write cache, optional feature unavailable)
   - Deprecated code paths
   - Performance warnings

3. **ERROR**: Error conditions requiring attention
   - Failed operations with user impact
   - Exceptions and error objects
   - Invalid state that prevents functionality

**Context Guidelines**:

Always include these fields when applicable:
- `component`: The component/module generating the log
- `terminalId`: Terminal identifier for terminal-related operations
- `workspacePath`: Workspace path for workspace operations
- `errorId`: Auto-generated unique error ID for tracking

**Example Patterns**:

```typescript
// Starting multi-step operation
logger.info("Starting workspace reset", {
  workspacePath,
  terminalCount: terminals.length,
});

// Operation milestone
logger.info("Destroyed terminal session", {
  workspacePath,
  terminalId: terminal.id,
  terminalName: terminal.name,
});

// Error with full context
logger.error("Failed to create worktree", error, {
  workspacePath,
  terminalId,
  worktreePath,
});

// Operation complete
logger.info("Workspace reset complete", { workspacePath });
```

### Benefits

1. **Structured Data**: JSON format is easily parsed by tools (jq, MCP servers)
2. **Context Preservation**: All relevant context attached to each log entry
3. **Error Tracking**: Unique error IDs for correlation across logs
4. **Component Tracing**: Component field enables filtering by module
5. **Debugging**: Rich context makes post-mortem debugging easier

### Log File Locations

- **Frontend Console**: `~/.loom/console.log` (JSON structured logs)
- **Daemon**: `~/.loom/daemon.log` (JSON structured logs)
- **Tauri**: `~/.loom/tauri.log` (Tauri application logs)
- **Terminal Output**: `/tmp/loom-terminal-{id}.out` (raw terminal output)

### Querying Logs

**Using jq**:
```bash
# Filter by component
jq 'select(.context.component == "worktree-manager")' ~/.loom/console.log

# Find errors
jq 'select(.level == "ERROR")' ~/.loom/console.log

# Track specific terminal
jq 'select(.context.terminalId == "terminal-1")' ~/.loom/console.log

# Find by error ID
jq 'select(.context.errorId == "ERR-abc123")' ~/.loom/console.log
```

**Using MCP Tools**:
```bash
# Read recent console logs
mcp__loom-logs__tail_daemon_log --lines=100

# Read terminal-specific logs
mcp__loom-logs__tail_terminal_log --terminal-id=terminal-1
```

### Migration Strategy

The logging system is being gradually adopted across the codebase:

**Phase 1 (Complete)**:
- ✅ Frontend Logger class (`src/lib/logger.ts`)
- ✅ Backend logging macros (`loom-daemon/src/logging.rs`)
- ✅ High-risk paths (workspace-reset, worktree-manager)

**Phase 2 (In Progress)**:
- Agent launcher (`src/lib/agent-launcher.ts`)
- Daemon terminal operations (`loom-daemon/src/terminal.rs`)
- Daemon IPC layer (`loom-daemon/src/ipc.rs`)

**Phase 3 (Planned)**:
- Remaining console.log statements converted gradually
- Log file rotation (keep last 10 files, 10MB each)
- Log aggregation dashboard (optional)

### Adding Structured Logging to New Code

1. **Import logger**:
   ```typescript
   import { Logger } from "./logger";
   const logger = Logger.forComponent("my-component");
   ```

2. **Replace console.log**:
   ```typescript
   // Before
   console.log(`[my-component] Created terminal ${id}`);

   // After
   logger.info("Created terminal", { terminalId: id });
   ```

3. **Add context**:
   Include all relevant IDs, paths, and state in the context object

4. **Use error method for exceptions**:
   ```typescript
   try {
     // operation
   } catch (error) {
     logger.error("Operation failed", error, { terminalId, path });
   }
   ```

## Git Workflow

### Branch Strategy

- `main`: Always stable, ready to release
- `feature/issue-X-description`: Feature branches from issues
- PR required for merge to main

### Commit Convention

```
<type>: <short description>

<longer description>

<footer>
```

Example:
```
Implement initial layout structure with terminal management

Build core UI layout with header, primary terminal view, mini terminal row...

Closes #2
```

### PR Process

1. Create feature branch from main
2. Implement feature
3. Test manually (`pnpm tauri:dev`)
4. **CRITICAL: Run `pnpm check:ci`** - This runs the exact same checks as CI
5. Fix any errors found by local CI checks
6. Create PR with detailed description
7. Merge after review

### IMPORTANT: AI Agent Pre-PR Checklist

**For all AI agents (Worker, Architect, Curator, Reviewer):**

Before creating or updating a Pull Request, you MUST run:

```bash
pnpm check:ci
```

This command runs the complete CI suite locally:
- Biome linting and formatting
- Rust formatting (rustfmt)
- Clippy with all CI flags (`--workspace --all-targets --all-features --locked -D warnings`)
- Cargo check
- Frontend build (TypeScript compilation + Vite)
- All tests (daemon integration tests)

**Why This Matters for AI Agents:**

1. **Prevent CI Failures**: Running `pnpm check:ci` catches issues locally before pushing
2. **Save Time**: Fix issues immediately instead of waiting for remote CI to fail
3. **Match CI Exactly**: Uses the exact same commands and flags as GitHub Actions
4. **Avoid Wasted Cycles**: Don't create PRs that will fail CI checks

**Common Mistakes AI Agents Make:**

- Running `cargo clippy` directly instead of `pnpm clippy` (misses CI flags)
- Running `biome check` without `--write` flag (doesn't auto-fix)
- Skipping tests or not running full build
- Not checking format issues before commit

**Required Before PR Creation:**

```bash
# Step 1: Run full CI suite locally
pnpm check:ci

# Step 2: If any errors, fix them and re-run
# (repeat until clean)

# Step 3: Commit changes
git add -A
git commit -m "Your commit message"

# Step 4: Push and create PR
git push
gh pr create ...
```

**If `pnpm check:ci` Fails:**

1. Read the error output carefully
2. Fix the issues (format strings, unused variables, type errors, etc.)
3. Run `pnpm check:ci` again
4. Only proceed with PR when it passes clean

## Future Architecture (Issues #3-5)

### Issue #3: Daemon Architecture

**Goal**: Background process managing all terminals

```
Tauri App (UI) ←─ IPC ─→ Daemon (Node.js) ←─→ Terminal Processes
```

### Issue #4: Terminal Display

**Goal**: Real terminal emulator in primary view

Technology candidates:
- xterm.js (battle-tested)
- zutty (modern, GPU-accelerated)
- Custom implementation

### Issue #5: AI Agent Integration

**Goal**: Claude agents working in terminals

```
Daemon → Spawn terminal with Claude
       → Claude reads/writes terminal
       → Creates git commits/PRs
       → Updates issue status
```

## Architecture Decision Records (ADRs)

Significant architectural decisions are documented in dedicated ADR files for easier reference and understanding.

**📖 See [docs/adr/README.md](docs/adr/README.md) for the complete ADR index**

### Quick Reference

**Core Architecture**:
- [ADR-0001: Observer Pattern for State Management](docs/adr/0001-observer-pattern-state-management.md) - Why Observer Pattern over Redux/MobX
- [ADR-0002: Vanilla TypeScript over React/Vue/Svelte](docs/adr/0002-vanilla-typescript-over-frameworks.md) - Why no frameworks
- [ADR-0008: tmux + Rust Daemon Architecture](docs/adr/0008-tmux-daemon-architecture.md) - Why tmux and Rust daemon

**Configuration & State**:
- [ADR-0003: Separate Configuration and State Files](docs/adr/0003-config-state-file-split.md) - Config vs runtime state
- [ADR-0007: Tauri IPC for Filesystem Operations](docs/adr/0007-tauri-ipc-for-filesystem-operations.md) - Why Rust IPC commands

**Workflows & Coordination**:
- [ADR-0004: Git Worktree Paths Inside Workspace](docs/adr/0004-worktree-paths-inside-workspace.md) - Sandbox-compatible worktree paths
- [ADR-0006: Label-Based Workflow Coordination](docs/adr/0006-label-based-workflow-coordination.md) - GitHub labels as state machine

**UI & Interaction**:
- [ADR-0005: HTML5 Drag API over Mouse Events](docs/adr/0005-html5-drag-api-over-mouse-events.md) - Native drag behavior

### Quick Answers

**Why Tauri over Electron?** Performance, security, size (~10MB vs ~100MB), modern web standards

**Why Map over Array for State?** O(1) lookups by ID, clear semantics, easy to extend

**Why Class for State?** Encapsulation, TypeScript type checking, familiar OOP patterns

**Why Separate displayedWorkspacePath?** Better UX - don't clear invalid input, show specific errors while preserving user typing

## Common Pitfalls

### 1. Forgetting to Call notify()

When adding a new state mutation method, always call `this.notify()`:

```typescript
updateTerminalStatus(id: string, status: TerminalStatus): void {
  const terminal = this.terminals.get(id);
  if (terminal) {
    terminal.status = status;
    this.notify(); // DON'T FORGET THIS
  }
}
```

### 2. Event Listener Memory Leaks

Our current pattern re-creates listeners on every render, which is fine for now but watch for:
- Listeners on `window` or `document` (persist across renders)
- Timers or intervals not cleaned up
- Long-lived references in closures

### 3. Missing Dark Mode Variants

Every color class needs a `dark:` variant:

```html
<!-- WRONG -->
<div class="bg-gray-100 text-gray-900">

<!-- RIGHT -->
<div class="bg-gray-100 dark:bg-gray-800 text-gray-900 dark:text-gray-100">
```

### 4. Inline Styles vs Tailwind

Prefer Tailwind classes over inline styles for theme support:

```html
<!-- WRONG (doesn't respect theme) -->
<div style="background-color: #1a1a1a">

<!-- RIGHT (respects theme) -->
<div class="bg-gray-900 dark:bg-gray-800">
```

## Questions to Ask When Adding Features

1. **State**: Does this need to be in `AppState`? Will other components need it?
2. **Rendering**: Is this a pure function? Can it be tested independently?
3. **Events**: Should this use delegation or direct listener?
4. **Types**: What TypeScript types/interfaces are needed?
5. **Theme**: Does this work in both light and dark mode?
6. **Performance**: Will this scale to 100+ terminals?

## Resources

- **Tauri Docs**: https://tauri.app/v1/guides/
- **TypeScript Handbook**: https://www.typescriptlang.org/docs/
- **TailwindCSS Docs**: https://tailwindcss.com/docs
- **GitHub Issues**: Track work and discuss architecture
- **CLAUDE.md**: You're reading it! Keep this updated.

## Maintaining This Document

This document should evolve as the project grows:

1. **When adding patterns**: Document the pattern and rationale in the relevant section
2. **When making architectural decisions**: Create an ADR in `docs/adr/` (see ADR template)
3. **When finding pitfalls**: Add to "Common Pitfalls"
4. **When removing code**: Update relevant sections and mark related ADRs as deprecated

**CLAUDE.md vs ADRs**:
- **CLAUDE.md**: Onboarding, high-level patterns, how-to guides, common tasks
- **ADRs**: Specific architectural decisions with context, alternatives, and tradeoffs

Keep this as a living document that helps both humans and AI understand the codebase deeply.

---

Last updated: Issue #19 (Terminal Configuration System) - Complete role-based terminal configuration with file-based role definitions, autonomous mode, and label-based workflow coordination
