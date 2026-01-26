# Loom API Reference

This document provides a comprehensive reference for Loom's APIs, including daemon CLI commands, Tauri IPC commands, frontend state management, and daemon IPC protocol.

## Table of Contents

- [Daemon CLI Commands](#daemon-cli-commands)
- [Tauri IPC Commands](#tauri-ipc-commands)
- [Frontend State API](#frontend-state-api)
- [Daemon IPC Protocol](#daemon-ipc-protocol)
- [MCP Server APIs](#mcp-server-apis)

## Daemon CLI Commands

The `loom-daemon` binary provides command-line interface for headless operations.

### `loom-daemon init`

Initialize a Loom workspace in a git repository.

**Synopsis:**
```bash
loom-daemon init [OPTIONS] [PATH]
```

**Arguments:**

| Argument | Type | Description | Default |
|----------|------|-------------|---------|
| `PATH` | String | Target directory to initialize | Current directory (`.`) |

**Options:**

| Flag | Type | Description |
|------|------|-------------|
| `--force` | Boolean | Overwrite existing `.loom` directory |
| `--dry-run` | Boolean | Preview changes without applying |
| `--defaults <PATH>` | String | Custom defaults directory path |

**Description:**

Initializes a Loom workspace by:
1. Validating the target is a git repository
2. Copying `.loom/` configuration from defaults
3. Installing repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .github/)
4. Updating `.gitignore` with Loom ephemeral patterns

**Examples:**

```bash
# Initialize current directory
loom-daemon init

# Initialize specific repository
loom-daemon init /path/to/repo

# Preview changes
loom-daemon init --dry-run

# Force reinitialization
loom-daemon init --force

# Use custom defaults
loom-daemon init --defaults ./org-defaults

# Combine flags
loom-daemon init --force --defaults ./custom /path/to/repo
```

**Exit Codes:**

| Code | Meaning |
|------|---------|
| `0` | Success - workspace initialized |
| `1` | Error - see stderr for details |

**Common Errors:**

| Error Message | Cause | Solution |
|---------------|-------|----------|
| "Not a git repository (no .git directory found)" | Target lacks `.git` | Run `git init` first |
| "Workspace already initialized (.loom directory exists)" | `.loom/` exists | Use `--force` or skip if already set up |
| "Failed to create .loom directory: Permission denied" | Insufficient permissions | Check ownership with `ls -la` |
| "Defaults directory not found" | Cannot locate defaults | Specify with `--defaults /path` |

**Implementation:**

- **File:** `loom-daemon/src/init.rs`
- **Function:** `initialize_workspace(workspace_path, defaults_path, force)`
- **CLI Parser:** `loom-daemon/src/main.rs` (Clap argument parsing)

**See Also:**
- [CLI Reference Guide](../guides/cli-reference.md) - Complete command documentation
- [Getting Started Guide](../guides/getting-started.md) - Installation walkthrough
- [CI/CD Setup Guide](../guides/ci-cd-setup.md) - Pipeline integration

## Tauri IPC Commands

Tauri commands provide a bridge between the TypeScript frontend and Rust backend, enabling filesystem access and system operations.

### Import

```typescript
import { invoke } from '@tauri-apps/api/tauri';
```

### Workspace Commands

#### `validate_git_repo`

Validates that a given path is a valid git repository.

**Signature:**
```typescript
function validate_git_repo(path: string): Promise<boolean>
```

**Parameters:**
- `path` (string) - Absolute path to validate

**Returns:**
- `Promise<boolean>` - `true` if valid git repo, `false` otherwise

**Example:**
```typescript
const isValid = await invoke<boolean>('validate_git_repo', {
  path: '/Users/username/projects/my-repo'
});

if (isValid) {
  console.log('Valid git repository');
}
```

**Errors:**
- Returns `false` if path doesn't exist
- Returns `false` if path is not a directory
- Returns `false` if `.git` directory doesn't exist

### Role File Commands

#### `list_role_files`

Lists available role files from both workspace-specific and default locations.

**Signature:**
```typescript
function list_role_files(workspacePath: string): Promise<string[]>
```

**Parameters:**
- `workspacePath` (string) - Path to workspace

**Returns:**
- `Promise<string[]>` - Array of role filenames (e.g., `["builder.md", "judge.md"]`)

**Resolution order:**
1. Workspace-specific: `.loom/roles/`
2. Default: `defaults/roles/`

**Example:**
```typescript
const roleFiles = await invoke<string[]>('list_role_files', {
  workspacePath: '/Users/username/projects/my-repo'
});

console.log('Available roles:', roleFiles);
// Output: ["driver.md", "builder.md", "judge.md", ...]
```

#### `read_role_file`

Reads the content of a role definition file.

**Signature:**
```typescript
function read_role_file(
  workspacePath: string,
  filename: string
): Promise<string>
```

**Parameters:**
- `workspacePath` (string) - Path to workspace
- `filename` (string) - Role filename (e.g., `"builder.md"`)

**Returns:**
- `Promise<string>` - File content with `{{workspace}}` variable replaced

**Example:**
```typescript
const content = await invoke<string>('read_role_file', {
  workspacePath: '/Users/username/projects/my-repo',
  filename: 'builder.md'
});

console.log('Role definition:', content);
```

#### `read_role_metadata`

Reads optional JSON metadata for a role.

**Signature:**
```typescript
function read_role_metadata(
  workspacePath: string,
  filename: string
): Promise<string | null>
```

**Parameters:**
- `workspacePath` (string) - Path to workspace
- `filename` (string) - Role filename base (e.g., `"builder.md"`)

**Returns:**
- `Promise<string | null>` - JSON metadata string, or `null` if no metadata file exists

**Example:**
```typescript
const metadata = await invoke<string | null>('read_role_metadata', {
  workspacePath: '/Users/username/projects/my-repo',
  filename: 'builder.md'
});

if (metadata) {
  const parsed = JSON.parse(metadata);
  console.log('Default interval:', parsed.defaultInterval);
}
```

### Label Management Commands

#### `ensure_labels_exist`

Ensures all required Loom labels exist in the GitHub repository.

**Signature:**
```typescript
function ensure_labels_exist(workspacePath: string): Promise<void>
```

**Parameters:**
- `workspacePath` (string) - Path to workspace (must be a git repo)

**Returns:**
- `Promise<void>`

**Example:**
```typescript
await invoke<void>('ensure_labels_exist', {
  workspacePath: '/Users/username/projects/my-repo'
});

console.log('GitHub labels synchronized');
```

**Labels created:**
- `loom:ready` - Issue ready for worker
- `loom:building` - Issue being worked on
- `loom:blocked` - Issue blocked by dependencies
- `loom:proposal` - Architect proposal awaiting approval
- `loom:review-requested` - PR ready for review
- `loom:reviewing` - PR currently under review
- `loom:pr` - PR approved
- `loom:urgent` - High-priority issue

#### `reset_github_labels`

Resets GitHub label state machine during workspace restart.

**Signature:**
```typescript
function reset_github_labels(): Promise<LabelResetResult>
```

**Returns:**
```typescript
interface LabelResetResult {
  issues_updated: number;
  prs_updated: number;
  errors: string[];
}
```

**Example:**
```typescript
const result = await invoke<LabelResetResult>('reset_github_labels');

console.log(`Reset ${result.issues_updated} issues and ${result.prs_updated} PRs`);
if (result.errors.length > 0) {
  console.warn('Errors:', result.errors);
}
```

**Behavior:**
- Removes `loom:building` from all open issues
- Replaces `loom:reviewing` with `loom:review-requested` on open PRs
- Non-critical operation - continues on error

### Console Logging Commands

#### `append_to_console_log`

Appends a log entry to `~/.loom/console.log`.

**Signature:**
```typescript
function append_to_console_log(entry: string): Promise<void>
```

**Parameters:**
- `entry` (string) - Log entry to append (typically JSON-formatted)

**Returns:**
- `Promise<void>`

**Example:**
```typescript
const logEntry = JSON.stringify({
  timestamp: new Date().toISOString(),
  level: 'INFO',
  message: 'User action',
  context: { action: 'button_click' }
});

await invoke<void>('append_to_console_log', { entry: logEntry });
```

## Frontend State API

The `AppState` class manages all application state using the Observer pattern.

### Import

```typescript
import { appState } from './lib/state';
```

### Workspace Management

#### `setWorkspace(path: string): void`

Sets the current workspace path.

```typescript
appState.setWorkspace('/Users/username/projects/my-repo');
```

#### `getWorkspace(): string | null`

Gets the current workspace path.

```typescript
const workspace = appState.getWorkspace();
```

#### `setDisplayedWorkspace(path: string): void`

Sets the displayed workspace path (may be invalid).

```typescript
appState.setDisplayedWorkspace('/Users/username/invalid-path');
```

#### `getDisplayedWorkspace(): string | null`

Gets the displayed workspace path.

```typescript
const displayed = appState.getDisplayedWorkspace();
```

### Terminal Management

#### `addTerminal(terminal: Terminal): void`

Adds a new terminal to state.

```typescript
appState.addTerminal({
  id: 'terminal-1',
  name: 'Worker 1',
  status: TerminalStatus.Idle,
  isPrimary: false,
  role: 'builder',
  roleConfig: {
    workerType: 'claude',
    roleFile: 'builder.md',
    targetInterval: 0,
    intervalPrompt: ''
  }
});
```

#### `removeTerminal(id: string): void`

Removes a terminal by ID.

```typescript
appState.removeTerminal('terminal-1');
```

#### `getTerminal(id: string): Terminal | undefined`

Gets a specific terminal by ID.

```typescript
const terminal = appState.getTerminal('terminal-1');
if (terminal) {
  console.log('Terminal status:', terminal.status);
}
```

#### `getTerminals(): Terminal[]`

Gets all terminals.

```typescript
const terminals = appState.getTerminals();
console.log(`${terminals.length} terminals`);
```

#### `setPrimary(id: string): void`

Sets the primary (selected) terminal.

```typescript
appState.setPrimary('terminal-1');
```

#### `getPrimary(): Terminal | null`

Gets the current primary terminal.

```typescript
const primary = appState.getPrimary();
if (primary) {
  console.log('Primary terminal:', primary.name);
}
```

#### `updateTerminalRole(id: string, role: string, config: RoleConfig): void`

Updates a terminal's role configuration.

```typescript
appState.updateTerminalRole('terminal-1', 'builder', {
  workerType: 'claude',
  roleFile: 'builder.md',
  targetInterval: 300000, // 5 minutes
  intervalPrompt: 'Continue working on open tasks'
});
```

### State Observation

#### `onChange(callback: () => void): () => void`

Registers a callback to be notified of state changes.

```typescript
const unsubscribe = appState.onChange(() => {
  console.log('State changed!');
  // Re-render UI
});

// Later, to unsubscribe:
unsubscribe();
```

### Agent Numbering

#### `getNextAgentNumber(): number`

Gets the next agent number and increments the counter.

```typescript
const num = appState.getNextAgentNumber();
console.log('Creating Agent', num);
```

#### `setNextAgentNumber(num: number): void`

Sets the agent number counter.

```typescript
appState.setNextAgentNumber(5);
```

#### `getCurrentAgentNumber(): number`

Gets the current agent number without incrementing.

```typescript
const current = appState.getCurrentAgentNumber();
```

### Types

```typescript
interface Terminal {
  id: string;
  name: string;
  status: TerminalStatus;
  isPrimary: boolean;
  role?: string;
  roleConfig?: RoleConfig;
  tmuxSession?: string;
  workingDirectory?: string;
  health?: 'healthy' | 'missing' | 'unknown';
  lastHealthCheck?: number;
}

interface RoleConfig {
  workerType: 'claude' | 'codex';
  roleFile: string;
  targetInterval: number;
  intervalPrompt: string;
}

enum TerminalStatus {
  Idle = 'idle',
  Busy = 'busy',
  NeedsInput = 'needs_input',
  Error = 'error',
  Stopped = 'stopped'
}
```

## Daemon IPC Protocol

The Loom daemon uses a Unix socket at `~/.loom/loom-daemon.sock` with JSON messages.

### Message Format

All messages are JSON objects with a `type` field:

```json
{
  "type": "MessageType",
  "payload": { ... }
}
```

### Request Messages

#### CreateTerminal

Creates a new terminal session.

```json
{
  "type": "CreateTerminal",
  "payload": {
    "id": "terminal-1",
    "working_directory": "/Users/username/projects/my-repo"
  }
}
```

**Response:**
```json
{
  "type": "TerminalCreated",
  "payload": {
    "id": "terminal-1",
    "tmux_session": "loom-terminal-1"
  }
}
```

#### SendInput

Sends input to a terminal.

```json
{
  "type": "SendInput",
  "payload": {
    "id": "terminal-1",
    "input": "ls -la\n"
  }
}
```

**Response:**
```json
{
  "type": "InputSent",
  "payload": {
    "id": "terminal-1"
  }
}
```

#### ReadOutput

Reads terminal output.

```json
{
  "type": "ReadOutput",
  "payload": {
    "id": "terminal-1"
  }
}
```

**Response:**
```json
{
  "type": "Output",
  "payload": {
    "id": "terminal-1",
    "output": "file1\nfile2\n"
  }
}
```

#### KillTerminal

Terminates a terminal session.

```json
{
  "type": "KillTerminal",
  "payload": {
    "id": "terminal-1"
  }
}
```

**Response:**
```json
{
  "type": "TerminalKilled",
  "payload": {
    "id": "terminal-1"
  }
}
```

#### ListTerminals

Lists all active terminals.

```json
{
  "type": "ListTerminals"
}
```

**Response:**
```json
{
  "type": "TerminalList",
  "payload": {
    "terminals": [
      {
        "id": "terminal-1",
        "tmux_session": "loom-terminal-1",
        "working_directory": "/path/to/workspace"
      }
    ]
  }
}
```

#### Ping

Health check.

```json
{
  "type": "Ping"
}
```

**Response:**
```json
{
  "type": "Pong"
}
```

### Error Responses

```json
{
  "type": "Error",
  "payload": {
    "message": "Terminal not found: terminal-99"
  }
}
```

## MCP Server APIs

Loom provides a unified MCP server (`mcp-loom`) for AI-powered testing and debugging. See [MCP Documentation](../mcp/README.md) for complete API reference.

### Quick Reference

**UI Tools:**
- `read_console_log` - Frontend console logs
- `read_state_file` - Application state
- `read_config_file` - Terminal configurations
- `trigger_force_start` - Start engine
- `trigger_factory_reset` - Reset to defaults
- `get_heartbeat` - Health status

**Log Tools:**
- `tail_daemon_log` - Daemon logs
- `tail_tauri_log` - Tauri logs
- `list_terminal_logs` - List terminal logs
- `tail_terminal_log` - Terminal-specific logs

**Terminal Tools:**
- `list_terminals` - All terminals
- `get_selected_terminal` - Primary terminal
- `get_terminal_output` - Terminal output
- `send_terminal_input` - Send input

See the [MCP Documentation](../mcp/) for detailed usage examples.

## Configuration File Formats

### .loom/config.json

Workspace configuration persisted across app restarts.

```json
{
  "nextAgentNumber": 4,
  "agents": [
    {
      "id": "terminal-1",
      "name": "Worker 1",
      "status": "idle",
      "isPrimary": true,
      "role": "builder",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "targetInterval": 0,
        "intervalPrompt": ""
      }
    }
  ]
}
```

### .loom/state.json

Runtime state (not persisted to config).

```json
{
  "terminals": {
    "terminal-1": {
      "id": "terminal-1",
      "tmuxSession": "loom-terminal-1",
      "workingDirectory": "/path/to/workspace",
      "health": "healthy",
      "lastHealthCheck": 1234567890
    }
  }
}
```

### Role Metadata (.json)

Optional metadata for role files.

```json
{
  "name": "Worker Bot",
  "description": "General development worker",
  "defaultInterval": 0,
  "defaultIntervalPrompt": "Continue working on open tasks",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
```

## See Also

- [Architecture Overview](../architecture/system-overview.md) - System architecture and data flow
- [Testing Guide](../guides/testing.md) - MCP testing and debugging
- [Code Quality Guide](../guides/code-quality.md) - Development workflows
