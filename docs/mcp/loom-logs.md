# MCP Loom Logs Server API Reference

The `mcp-loom-logs` server provides tools for accessing Loom's various log files including daemon logs, Tauri application logs, and individual terminal output logs.

## Server Information

- **Name**: `loom-logs`
- **Version**: `0.1.0`
- **Package**: `mcp-loom-logs`
- **Entry Point**: `mcp-loom-logs/src/index.ts`

## Overview

This MCP server enables AI agents to:
- Read Loom daemon logs for backend debugging
- Read Tauri application logs for frontend debugging
- List and read individual terminal output logs
- Monitor system-level activity and errors

All log files are stored in standard locations (`~/.loom/` and `/tmp/`).

---

## Tools

### `tail_daemon_log`

Tail the Loom daemon log file (`~/.loom/daemon.log`). Shows recent daemon activity including terminal creation, IPC requests, and errors.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `lines` | number | No | 100 | Number of lines to show |

**Returns:**

Plain text containing the last N lines from daemon log, prefixed with header.

**Output Format:**
```
=== Daemon Log (last 100 lines) ===

[2025-10-15 05:05:06] [INFO] Received CreateTerminal request
[2025-10-15 05:05:06] [INFO] Created tmux session: loom-terminal-2
[2025-10-15 05:05:06] [DEBUG] Terminal created with ID: terminal-2
[2025-10-15 05:05:07] [INFO] Received SendInput request for terminal-2
```

**Error Conditions:**

- **File Not Found**: Returns message indicating daemon hasn't been started yet or logging isn't configured
- **Not a File**: Returns error if path exists but isn't a regular file
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read last 50 lines
mcp__loom-logs__tail_daemon_log({ lines: 50 })

// Read default 100 lines
mcp__loom-logs__tail_daemon_log()
```

**Use Cases:**
- Debugging terminal creation issues
- Monitoring IPC communication between frontend and daemon
- Investigating daemon crashes or errors
- Verifying tmux session management

---

### `tail_tauri_log`

Tail the Loom Tauri application log file (`~/.loom/tauri.log`). Shows frontend activity, state changes, and UI errors.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `lines` | number | No | 100 | Number of lines to show |

**Returns:**

Plain text containing the last N lines from Tauri log, prefixed with header.

**Output Format:**
```
=== Tauri Log (last 100 lines) ===

[2025-10-15 05:05:05] [INFO] Application started
[2025-10-15 05:05:05] [INFO] Workspace selected: /Users/user/GitHub/loom
[2025-10-15 05:05:06] [INFO] Starting workspace with 8 agents
[2025-10-15 05:05:06] [DEBUG] Created terminal: terminal-1 (Architect)
```

**Error Conditions:**

- **File Not Found**: Returns message indicating Loom app hasn't been started yet
- **Not a File**: Returns error if path exists but isn't a regular file
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read last 200 lines
mcp__loom-logs__tail_tauri_log({ lines: 200 })

// Read default 100 lines
mcp__loom-logs__tail_tauri_log()
```

**Use Cases:**
- Debugging UI state management
- Monitoring workspace operations (start, reset)
- Investigating frontend errors
- Tracking agent launch sequence

---

### `list_terminal_logs`

List all available terminal output logs (`/tmp/loom-*.out`). Each terminal's output is captured to a separate file.

**Parameters:**

None

**Returns:**

Plain text list of available terminal log files.

**Output Format:**
```
=== Available Terminal Logs ===

/tmp/loom-terminal-1.out
/tmp/loom-terminal-2.out
/tmp/loom-terminal-3.out
/tmp/loom-terminal-4.out
/tmp/loom-terminal-5.out
/tmp/loom-terminal-6.out
```

**Empty Case:**
```
No terminal logs found. Terminals may not have been created yet.
```

**Error Conditions:**

- **No Logs**: Returns message if no terminal log files exist
- **Read Error**: Returns empty list if unable to read `/tmp/` directory

**Example:**
```typescript
// List all terminal logs
mcp__loom-logs__list_terminal_logs()
```

**Use Cases:**
- Discovering which terminals are actively logging
- Finding terminal IDs for use with `tail_terminal_log`
- Verifying terminal creation after factory reset
- Checking if terminals were successfully launched

---

### `tail_terminal_log`

Tail a specific terminal's output log. Terminal IDs are like `terminal-1`, `terminal-2`, etc.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `terminal_id` | string | **Yes** | - | Terminal ID (e.g., `terminal-1`) |
| `lines` | number | No | 100 | Number of lines to show |

**Returns:**

Plain text containing the last N lines from terminal output log, prefixed with header.

**Output Format:**
```
=== Terminal terminal-2 Log (last 100 lines) ===

[launchAgentInTerminal] START - terminalId=terminal-2...
[launchAgentInTerminal] Worktree setup complete
[launchAgentInTerminal] Sending command: claude --dangerously-skip-permissions
Claude Code v1.2.3
Ready to assist with your codebase!

>
```

**Error Conditions:**

- **Missing Parameter**: Returns error if `terminal_id` not provided
- **File Not Found**: Returns message indicating terminal hasn't been created or was closed
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read last 50 lines from terminal-2
mcp__loom-logs__tail_terminal_log({
  terminal_id: "terminal-2",
  lines: 50
})

// Read default 100 lines from terminal-1
mcp__loom-logs__tail_terminal_log({
  terminal_id: "terminal-1"
})
```

**Use Cases:**
- Verifying Claude Code launched successfully
- Checking for bypass permissions acceptance
- Debugging agent command execution
- Monitoring agent activity and output
- Investigating terminal errors or hangs

---

## File Locations

| File | Path | Purpose |
|------|------|---------|
| Daemon Log | `~/.loom/daemon.log` | Backend daemon activity and IPC |
| Tauri Log | `~/.loom/tauri.log` | Frontend application activity |
| Terminal Logs | `/tmp/loom-{terminal_id}.out` | Individual terminal output |

---

## Common Patterns

### Debugging Factory Reset

```typescript
// 1. List available terminals
const terminals = await mcp__loom-logs__list_terminal_logs();
// Should show 8 terminals after default factory reset

// 2. Check each terminal for successful agent launch
for (const terminalId of ["terminal-2", "terminal-3", "terminal-4", "terminal-5", "terminal-6"]) {
  const log = await mcp__loom-logs__tail_terminal_log({
    terminal_id: terminalId,
    lines: 50
  });

  // Look for:
  // - "Claude Code" or "Codex" startup message
  // - No "command not found" errors
  // - No stuck bypass permissions prompts
}

// 3. Check daemon for terminal creation sequence
const daemonLog = await mcp__loom-logs__tail_daemon_log({ lines: 200 });
// Look for CreateTerminal requests and successful responses
```

### Investigating Agent Launch Failures

```typescript
// 1. Check Tauri log for launch sequence
const tauriLog = await mcp__loom-logs__tail_tauri_log({ lines: 100 });
// Look for [launchAgentInTerminal] and [launchCodexAgent] messages

// 2. Check daemon log for IPC issues
const daemonLog = await mcp__loom-logs__tail_daemon_log({ lines: 100 });
// Look for SendInput requests and responses

// 3. Check specific terminal output
const terminalLog = await mcp__loom-logs__tail_terminal_log({
  terminal_id: "terminal-3",
  lines: 50
});
// Look for error messages or stuck prompts
```

### Monitoring System Health

```typescript
// Check daemon is running
const daemonLog = await mcp__loom-logs__tail_daemon_log({ lines: 10 });
if (daemonLog.includes("Log file not found")) {
  // Daemon is not running
}

// Check Tauri app is running
const tauriLog = await mcp__loom-logs__tail_tauri_log({ lines: 10 });
if (tauriLog.includes("Log file not found")) {
  // Tauri app is not running
}

// Check terminal count
const terminals = await mcp__loom-logs__list_terminal_logs();
// Compare with expected count
```

### Analyzing Command Timing Issues

```typescript
// Get terminal output to see command concatenation
const terminalLog = await mcp__loom-logs__tail_terminal_log({
  terminal_id: "terminal-2",
  lines: 200
});

// Look for issues like:
// - "claude --dangerously-skip-permissions2" (concatenated with "2")
// - Multiple commands on one line
// - Missing newlines between commands

// Cross-reference with Tauri log timing
const tauriLog = await mcp__loom-logs__tail_tauri_log({ lines: 200 });
// Check delay between send_terminal_input calls
```

---

## Error Handling

All tools return errors as plain text with appropriate context:

**File Not Found Example:**
```
Log file not found: ~/.loom/daemon.log

This usually means:
- Loom hasn't been started yet, or
- Logging to file hasn't been configured
```

**Empty Log Example:**
```
(empty log file)
```

Common error types:
- **ENOENT**: File not found (common for logs before app starts)
- **EACCES**: Permission denied
- **EISDIR**: Path is a directory, not a file

---

## Log Rotation

Currently, Loom logs are **not rotated**. Long-running instances may accumulate large log files. Consider:

- Terminal logs in `/tmp/` are cleared on system restart
- Daemon and Tauri logs in `~/.loom/` persist across restarts
- Manual cleanup may be needed: `rm ~/.loom/*.log`

---

## See Also

- [MCP Loom UI](./loom-ui.md) - UI interaction and console logs
- [MCP Loom Terminals](./loom-terminals.md) - Terminal management and IPC
- [MCP Overview](./README.md) - Introduction to Loom MCP servers
