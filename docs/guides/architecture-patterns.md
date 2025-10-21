# Architecture Patterns

## 1. Observer Pattern (State Management)

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

## 2. Pure Functions (UI Rendering)

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

## 3. Event Delegation

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

## 4. Reactive Rendering

The render cycle:

```
State Change → notify() → onChange callbacks → render() → setupEventListeners()
```

**Important**: `setupEventListeners()` is called after every render to re-attach handlers to new DOM elements. This is intentional and works because:
1. Old elements are removed (garbage collected)
2. New elements need fresh event listeners
3. Event delegation minimizes performance impact

## 5. Tauri IPC (Inter-Process Communication)

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

## 6. Persistent Configuration

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
      "name": "Builder 1",
      "status": "idle",
      "isPrimary": false,
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
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
  roleFile: 'builder.md',
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

## 7. Terminal Configuration System

**Files**: `src/lib/terminal-settings-modal.ts`, `src-tauri/src/main.rs` (role file commands)

The terminal configuration system allows users to assign specialized roles to each terminal through a settings modal.

**Role Definition Structure**:

Each role consists of two files:
- **`.md` file** (required): The role definition text with markdown formatting
- **`.json` file** (optional): Metadata with default settings

**Role Metadata Schema**:
```json
{
  "name": "Builder Bot",
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
| **Driver** | `driver.md` | No | N/A | Plain shell environment, no specialized role |
| **Builder** | `builder.md` | No | 0 (manual) | General development worker for features, bugs, and refactoring |
| **Issues** | `issues.md` | No | 0 (manual) | Specialist for creating well-structured GitHub issues |
| **Judge** | `judge.md` | Yes | 5 min | Code review specialist for thorough PR reviews |
| **Architect** | `architect.md` | Yes | 15 min | System architecture and technical decision making |
| **Curator** | `curator.md` | Yes | 5 min | Issue maintenance and quality improvement |

**Autonomous Mode**:
- When `targetInterval > 0`, the terminal will automatically execute the `intervalPrompt` at regular intervals
- Example: Judge bot runs every 5 minutes with prompt "Find and review open PRs with loom:review-requested label"
- Allows terminals to work autonomously without user intervention
- Recommended for Curator, Judge, and Architect roles

**Label-based Workflow Coordination**:

Roles coordinate work through GitHub labels with two human approval gates (see [WORKFLOWS.md](../../WORKFLOWS.md) for complete details):

1. **Architect** creates issues with `loom:architect` label
2. **Human approval (Gate 1)**: User reviews and removes label to approve proposal
3. **Curator** finds unlabeled issues, enhances them, marks as `loom:curated`
4. **Human approval (Gate 2)**: User reviews and changes `loom:curated` to `loom:issue` to authorize work
5. **Builder** claims `loom:issue` issues, implements, creates PR with `loom:review-requested`
6. **Judge** finds `loom:review-requested` PRs, reviews, approves/requests changes
7. **Human merges** approved PRs

The two approval gates ensure human judgment guides the autonomous workflow:
- **Gate 1**: Strategic alignment - "Should we pursue this direction?"
- **Gate 2**: Resource allocation - "Is this worth implementing now?"

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

See [defaults/roles/README.md](../../defaults/roles/README.md) for detailed guidance on creating custom roles.

## 8. Git Worktrees and Sandbox Compatibility

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
pnpm worktree 42

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
- To work on an issue: `pnpm worktree <issue-number>` creates `.loom/worktrees/issue-{number}`
- Helper script prevents nested worktrees and ensures proper paths
- Multiple agents can work simultaneously by each claiming their own issue
- Worktrees are named semantically by issue number, not terminal ID

**For agents**: Use `pnpm worktree <issue>` when claiming an issue. Create feature branches in your worktree.

### Context 2: Human Developers Working on Loom's Codebase (Dogfooding)

When **human developers** (not agents) want to work on Loom issues manually outside the app, use the worktree helper script:

```bash
# ✅ CORRECT - Use the helper script
pnpm worktree 84

# ✅ With custom branch name
pnpm worktree 84 my-custom-branch

# ✅ Check if you're already in a worktree
pnpm worktree --check

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
pnpm worktree 42
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/issue-42

# Custom branch name
pnpm worktree 42 fix-critical-bug
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/fix-critical-bug

# Check current worktree status
pnpm worktree --check
# → Shows: Current worktree path and branch (or confirms you're in main)

# Show help
pnpm worktree --help
```

**When to Use the Helper Script**:

- **Human developers** working on Loom codebase issues manually
- **NOT for agents** (they already have worktrees)
- **NOT needed when using Loom itself** (it creates worktrees automatically)

**Human Developer Workflow**:

```bash
# 1. Starting work on issue #123 (from main workspace)
cd /Users/rwalters/GitHub/loom
pnpm worktree 123
cd .loom/worktrees/issue-123
# Work on the issue, commit, push, create PR

# 2. Check if you're in a worktree
pnpm worktree --check

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
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:in-progress"
pnpm worktree 42
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
## 9. Command Module Organization

**Files**: `src-tauri/src/commands/`, `src-tauri/src/main.rs`

Tauri commands are organized into domain-specific modules rather than a monolithic main.rs file. This pattern improves maintainability, reduces merge conflicts, and makes the codebase easier to navigate.

### Module Structure

```
src-tauri/src/
├── main.rs                # Minimal entry point (command registration, app setup)
├── commands/
│   ├── mod.rs             # Module index with re-exports
│   ├── terminal.rs        # Terminal management (12 commands)
│   ├── workspace.rs       # Workspace operations (7 commands)
│   ├── config.rs          # Config/state I/O (5 commands)
│   ├── github.rs          # GitHub integration (5 commands)
│   ├── project.rs         # Project creation (2 commands)
│   ├── daemon.rs          # Daemon health checks (2 commands)
│   ├── filesystem.rs      # File operations (3 commands)
│   ├── system.rs          # System checks (5 commands)
│   └── ui.rs              # UI events (5 commands)
└── menu.rs                # Menu building and event handling
```

### Why Domain-Specific Modules?

**Before Refactoring** (Issue #421):
- 1,885 lines in main.rs
- All 51 commands in one file
- High risk of merge conflicts
- Difficult to navigate and locate commands

**After Refactoring**:
- 290 lines in main.rs (85% reduction)
- Commands grouped by domain
- Clear boundaries reduce conflicts
- Easy to find related functionality

### Implementation Pattern

**1. Command Module** (`src-tauri/src/commands/workspace.rs`):
```rust
use std::path::Path;

#[tauri::command]
pub fn validate_git_repo(path: &str) -> Result<bool, String> {
    let workspace_path = Path::new(path);
    
    if !workspace_path.exists() {
        return Err("Path does not exist".to_string());
    }
    
    if !workspace_path.is_dir() {
        return Err("Path is not a directory".to_string());
    }
    
    let git_path = workspace_path.join(".git");
    if !git_path.exists() {
        return Err("Not a git repository".to_string());
    }
    
    Ok(true)
}

// Helper functions (also pub for use by other modules if needed)
pub fn find_git_root() -> Option<PathBuf> {
    // Implementation...
}
```

**2. Module Index** (`src-tauri/src/commands/mod.rs`):
```rust
pub mod config;
pub mod daemon;
pub mod filesystem;
pub mod github;
pub mod project;
pub mod system;
pub mod terminal;
pub mod ui;
pub mod workspace;

// Re-export all command functions
pub use config::*;
pub use daemon::*;
pub use filesystem::*;
pub use github::*;
pub use project::*;
pub use system::*;
pub use terminal::*;
pub use ui::*;
pub use workspace::*;
```

**3. Main Entry Point** (`src-tauri/src/main.rs`):
```rust
mod commands;
mod menu;

#[allow(clippy::wildcard_imports)]
use commands::*;

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            // Legacy commands
            get_cli_workspace,
            // System commands
            check_system_dependencies,
            get_env_var,
            // ... all 51 commands registered here ...
        ])
        .run(tauri::generate_context!())
}
```

### Domain Boundaries

| Module | Responsibility | Key Commands |
|--------|---------------|--------------|
| `terminal.rs` | Terminal lifecycle and I/O | `create_terminal`, `send_terminal_input`, `get_terminal_output` |
| `workspace.rs` | Git repo validation and setup | `validate_git_repo`, `initialize_loom_workspace` |
| `config.rs` | Configuration and state persistence | `read_config`, `write_state`, `read_role_file` |
| `github.rs` | GitHub API integration | `check_label_exists`, `reset_github_labels` |
| `project.rs` | Project creation workflows | `create_local_project`, `create_github_repository` |
| `daemon.rs` | Daemon health monitoring | `check_daemon_health`, `get_daemon_status` |
| `filesystem.rs` | File I/O operations | `read_text_file`, `write_file`, `append_to_console_log` |
| `system.rs` | System dependency checks | `check_system_dependencies`, `check_claude_code` |
| `ui.rs` | UI events and triggers | `emit_event`, `trigger_start`, `trigger_factory_reset` |

### Guidelines for Adding Commands

1. **Choose the Right Module**: Place commands in the module that matches their primary responsibility
2. **Keep Functions Public**: Command functions and shared helpers must be `pub`
3. **Re-export in mod.rs**: Add `pub use module_name::*;` for new modules
4. **Register in main.rs**: Add command to `generate_handler![]` macro
5. **Maintain Domain Purity**: Avoid cross-domain dependencies where possible

### Benefits

1. **Reduced Merge Conflicts**: Smaller files mean less chance of simultaneous edits
2. **Improved Navigation**: Jump to domain module instead of searching large file
3. **Clear Ownership**: Each module has a focused responsibility
4. **Scalability**: Easy to add new domains as command count grows
5. **Better Testing**: Can test domain modules independently
6. **Code Review**: Reviewers can focus on specific domains

### Example: Adding a New Command

```rust
// 1. Add to appropriate module (commands/workspace.rs)
#[tauri::command]
pub fn check_workspace_clean(path: &str) -> Result<bool, String> {
    // Implementation
    Ok(true)
}

// 2. Re-export from commands/mod.rs (already done via `pub use workspace::*;`)

// 3. Register in main.rs
.invoke_handler(tauri::generate_handler![
    // ... existing commands ...
    check_workspace_clean,  // Add here
])
```

**See Also**:
- [docs/guides/common-tasks.md](common-tasks.md#adding-a-new-tauri-command) - Step-by-step guide for adding commands
- [docs/architecture/system-overview.md](../architecture/system-overview.md) - Full system architecture
