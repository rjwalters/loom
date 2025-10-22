# MCP Loom UI Server API Reference

The `mcp-loom-ui` server provides tools for interacting with the Loom application's UI layer, console logs, and workspace state.

## Server Information

- **Name**: `loom-ui`
- **Version**: `0.1.0`
- **Package**: `mcp-loom-ui`
- **Entry Point**: `mcp-loom-ui/src/index.ts`

## Overview

This MCP server enables AI agents (like Claude Code) to:
- Read browser console logs from the Loom Tauri application
- Monitor application state and configuration
- Trigger workspace operations (start, reset)
- Check application health status
- Get random file paths from the workspace for code review

All file operations use `~/.loom/` directory for logs and state files.

---

## Tools

### `read_console_log`

Read the Loom browser console log to see JavaScript errors, console.log output, and debugging information.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `lines` | number | No | 100 | Number of recent lines to return |

**Returns:**

Plain text containing the last N lines from `~/.loom/console.log`.

**Output Format:**
```
[2025-10-15T05:05:06.088Z] [INFO] [launchAgentsForTerminals] Starting agent launch...
[2025-10-15T05:05:06.814Z] [INFO] [launchAgentInTerminal] Worktree setup complete
```

**Error Conditions:**

- **File Not Found**: Returns message indicating console log file doesn't exist (app may need to enable logging)
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read last 50 lines
mcp__loom-ui__read_console_log({ lines: 50 })

// Read default 100 lines
mcp__loom-ui__read_console_log()
```

**Use Cases:**
- Debugging factory reset and agent launch processes
- Monitoring UI state changes and errors
- Tracking console output from frontend JavaScript

---

### `read_state_file`

Read the current Loom state file (`.loom/state.json`) to see terminal state, agent numbers, and selected terminal.

**Parameters:**

None

**Returns:**

JSON string containing the workspace state.

**Output Format:**
```json
{
  "terminals": [
    {
      "configId": "terminal-1",
      "id": "loom-terminal-1",
      "name": "Architect",
      "status": "idle",
      "isPrimary": true,
      "worktreePath": "/path/to/workspace/.loom/worktrees/terminal-1"
    }
  ],
  "selectedTerminalId": "loom-terminal-1",
  "nextAgentNumber": 2
}
```

**Error Conditions:**

- **File Not Found**: Returns message "State file not found. Workspace may not be initialized."
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read state file
mcp__loom-ui__read_state_file()
```

**Use Cases:**
- Verifying terminal creation after factory reset
- Checking active terminals and their IDs
- Monitoring worktree paths for debugging

---

### `read_config_file`

Read the current Loom config file (`.loom/config.json`) to see terminal configurations and role settings.

**Parameters:**

None

**Returns:**

JSON string containing the workspace configuration.

**Output Format:**
```json
{
  "nextAgentNumber": 9,
  "agents": [
    {
      "configId": "terminal-1",
      "id": "__needs_session__",
      "name": "Architect",
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "architect.md",
        "targetInterval": 900000,
        "intervalPrompt": "Scan codebase and create improvement suggestions"
      },
      "worktreePath": "",
      "theme": "lavender"
    }
  ]
}
```

**Error Conditions:**

- **File Not Found**: Returns message "Config file not found. Workspace may not be initialized."
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read config file
mcp__loom-ui__read_config_file()
```

**Use Cases:**
- Verifying configuration after reset
- Checking terminal roles and intervals
- Debugging agent launch settings

---

### `get_heartbeat`

Get app heartbeat status - checks if Loom is running and actively logging.

**Parameters:**

None

**Returns:**

JSON object with heartbeat information.

**Output Format:**
```json
{
  "status": "healthy",
  "message": "Last log entry was 3s ago",
  "lastLogTime": "2025-10-15T05:05:06.088Z",
  "logCount": 1523,
  "recentLogs": [
    "[2025-10-15T05:05:03.088Z] [INFO] Agent launched",
    "[2025-10-15T05:05:06.088Z] [INFO] Worktree ready"
  ]
}
```

**Status Values:**

| Status | Condition | Description |
|--------|-----------|-------------|
| `healthy` | Last log < 10s ago | App is actively logging |
| `active` | Last log < 60s ago | App is running normally |
| `idle` | Last log < 5min ago | App is running but quiet |
| `stale` | Last log > 5min ago | App may have frozen |
| `not_running` | Log file not found | App is not running |
| `unknown` | Cannot parse logs | Unable to determine status |

**Error Conditions:**

- **File Not Found**: Returns `status: "not_running"` with appropriate message
- **Parse Error**: Returns `status: "unknown"` with error details

**Example:**
```typescript
// Check if app is running
mcp__loom-ui__get_heartbeat()
```

**Use Cases:**
- Verifying Loom is running before triggering operations
- Monitoring app health during testing
- Detecting frozen or crashed app instances

---

### `get_random_file`

Get a random file path from the workspace. Respects `.gitignore` and excludes common build artifacts.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `includePatterns` | string[] | No | `["**/*"]` | Glob patterns to include (e.g., `["src/**/*.ts"]`) |
| `excludePatterns` | string[] | No | `[]` | Additional glob patterns to exclude beyond defaults |

**Default Exclusions:**

The following patterns are always excluded:
- `**/node_modules/**`
- `**/.git/**`
- `**/dist/**`
- `**/build/**`
- `**/target/**`
- `**/.loom/worktrees/**`
- `**/*.log`
- `**/package-lock.json`
- `**/pnpm-lock.yaml`
- `**/yarn.lock`

**Returns:**

Absolute path to a randomly selected file from the workspace.

**Output Format:**
```
/Users/username/GitHub/loom/src/lib/state.ts
```

**Error Conditions:**

- **No Files Found**: Returns message "No files found matching the criteria"
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Get any random file (default behavior)
mcp__loom-ui__get_random_file()

// Get random TypeScript file from src/
mcp__loom-ui__get_random_file({
  includePatterns: ["src/**/*.ts"]
})

// Get random file, excluding test files
mcp__loom-ui__get_random_file({
  excludePatterns: ["**/*.test.ts", "**/*.spec.ts"]
})

// Get random Rust file
mcp__loom-ui__get_random_file({
  includePatterns: ["**/*.rs"]
})
```

**Use Cases:**
- **Critic agent**: Randomly select files to review for code quality issues
- **Refactoring**: Find random files to improve or modernize
- **Code exploration**: Discover unfamiliar parts of the codebase
- **Random testing**: Pick files for spot checks or manual review

**How It Works:**

1. Scans the workspace directory using glob patterns
2. Filters out files matching default exclusions
3. Respects `.gitignore` rules if present
4. Applies custom include/exclude patterns
5. Randomly selects one file from the filtered list
6. Returns the absolute path

**Notes:**

- Respects workspace `.gitignore` automatically
- All paths relative to `LOOM_WORKSPACE` environment variable
- Only returns files, not directories
- Does not follow symbolic links

---

### `trigger_start`

Start the Loom engine using EXISTING workspace config (`.loom/config.json`). Shows confirmation dialog before creating terminals and launching agents.

**Parameters:**

None

**Returns:**

Status message indicating the command was written to the MCP command file.

**Behavior:**

1. Uses current `.loom/config.json` (does NOT reset or overwrite)
2. Shows confirmation dialog to user
3. Creates terminals based on config
4. Launches agents with configured roles
5. Resets GitHub labels (`loom:building` → removed, `loom:reviewing` → `loom:review-requested`)

**Error Conditions:**

- **No Workspace**: Fails if workspace is not selected
- **File Write Error**: Returns error if unable to write MCP command file

**Example:**
```typescript
// Start engine with existing config (shows confirmation)
mcp__loom-ui__trigger_start()
```

**Use Cases:**
- Restarting terminals after app restart or crash
- Resuming work with current configuration
- Testing workspace start flow

**Note:** This command writes to `~/.loom/mcp-command.json` for the Loom app to pick up. File-based IPC is currently implemented in the Loom app.

---

### `trigger_force_start`

Start the Loom engine using existing config WITHOUT confirmation dialog. Same as `trigger_start` but bypasses user confirmation.

**Parameters:**

None

**Returns:**

Status message indicating the command was written to the MCP command file.

**Behavior:**

Same as `trigger_start` but:
- **No confirmation dialog** - executes immediately
- Useful for MCP automation and testing

**Error Conditions:**

- **No Workspace**: Fails if workspace is not selected
- **File Write Error**: Returns error if unable to write MCP command file

**Example:**
```typescript
// Start engine without confirmation (automated testing)
mcp__loom-ui__trigger_force_start()
```

**Use Cases:**
- Automated testing and CI workflows
- MCP automation scripts
- When you're certain the user wants to start

**Warning:** Use with caution - bypasses user confirmation!

---

### `trigger_factory_reset`

Reset workspace to factory defaults by overwriting `.loom/config.json` with `defaults/config.json`. Shows confirmation dialog.

**Parameters:**

None

**Returns:**

Status message indicating the command was written to the MCP command file.

**Behavior:**

1. Shows confirmation dialog to user
2. Overwrites `.loom/config.json` with `defaults/config.json`
3. **Does NOT auto-start** - user must run `trigger_start` or `trigger_force_start` afterward

**Two-Step Reset Workflow:**
```typescript
// Step 1: Reset config to defaults
mcp__loom-ui__trigger_factory_reset()

// Step 2: Start engine with new config
mcp__loom-ui__trigger_force_start()
```

**Error Conditions:**

- **No Workspace**: Fails if workspace is not selected
- **File Write Error**: Returns error if unable to write MCP command file

**Example:**
```typescript
// Reset to factory defaults (shows confirmation)
mcp__loom-ui__trigger_factory_reset()
```

**Use Cases:**
- Resetting configuration to clean state for testing
- Recovering from corrupted config
- Testing default agent lineup

**Important:** After factory reset, you must separately start the engine. This is intentional to prevent accidental terminal creation.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LOOM_WORKSPACE` | `~/GitHub/loom` | Workspace path for reading state/config files |

---

## File Locations

| File | Path | Purpose |
|------|------|---------|
| Console Log | `~/.loom/console.log` | Browser console output |
| State File | `{workspace}/.loom/state.json` | Current terminal state |
| Config File | `{workspace}/.loom/config.json` | Terminal configurations |
| MCP Command | `~/.loom/mcp-command.json` | File-based IPC for UI commands |

---

## Common Patterns

### Testing Factory Reset

```typescript
// 1. Check app is running
const heartbeat = await mcp__loom-ui__get_heartbeat();
// Verify status is "healthy" or "active"

// 2. Trigger factory reset
await mcp__loom-ui__trigger_factory_reset();

// 3. Trigger start without confirmation
await mcp__loom-ui__trigger_force_start();

// 4. Monitor console logs
const logs = await mcp__loom-ui__read_console_log({ lines: 200 });
// Look for agent launch messages

// 5. Verify state
const state = await mcp__loom-ui__read_state_file();
// Check all terminals created with correct IDs
```

### Debugging Agent Launch Issues

```typescript
// 1. Read recent console logs
const logs = await mcp__loom-ui__read_console_log({ lines: 100 });
// Look for errors or warnings

// 2. Check terminal state
const state = await mcp__loom-ui__read_state_file();
// Verify terminals have worktreePath set

// 3. Check configuration
const config = await mcp__loom-ui__read_config_file();
// Verify role files and worker types
```

### Monitoring App Health

```typescript
// Check if app is responsive
const heartbeat = await mcp__loom-ui__get_heartbeat();

if (heartbeat.status === "not_running") {
  // App needs to be started
} else if (heartbeat.status === "stale") {
  // App may have frozen - check logs
  const logs = await mcp__loom-ui__read_console_log();
}
```

---

## Error Handling

All tools return errors as plain text with `isError: true` flag:

```typescript
{
  content: [
    {
      type: "text",
      text: "Error: Failed to read console log: ENOENT"
    }
  ],
  isError: true
}
```

Common error types:
- **ENOENT**: File not found
- **EACCES**: Permission denied
- **Parse Error**: Invalid JSON in state/config files

---

## See Also

- [MCP Loom Logs](./loom-logs.md) - Daemon and terminal log access
- [MCP Loom Terminals](./loom-terminals.md) - Terminal management and IPC
- [MCP Overview](./README.md) - Introduction to Loom MCP servers
