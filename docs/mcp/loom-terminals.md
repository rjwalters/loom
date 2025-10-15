# MCP Loom Terminals Server API Reference

The `mcp-loom-terminals` server provides tools for interacting with Loom terminal sessions via the daemon IPC interface and terminal output logs.

## Server Information

- **Name**: `loom-terminals`
- **Version**: `0.1.0`
- **Package**: `mcp-loom-terminals`
- **Entry Point**: `mcp-loom-terminals/src/index.ts`

## Overview

This MCP server enables AI agents to:
- List active terminal sessions with metadata
- Read terminal output in real-time
- Send input to terminals (commands, text)
- Query the currently selected terminal
- Interact with the Loom daemon via IPC

This server communicates directly with the Loom daemon via Unix socket.

---

## Tools

### `list_terminals`

List all active Loom terminal sessions with their IDs, names, roles, and working directories.

**Parameters:**

None

**Returns:**

Plain text list of active terminals with detailed information.

**Output Format:**
```
=== Active Loom Terminals (8) ===

• ID: terminal-1
  Name: Architect
  Role: claude-code-worker
  Working Dir: /Users/user/GitHub/loom
  Session: loom-terminal-1

• ID: terminal-2
  Name: Curator
  Role: claude-code-worker
  Working Dir: /Users/user/GitHub/loom/.loom/worktrees/terminal-2
  Session: loom-terminal-2

• ID: terminal-3
  Name: Reviewer
  Role: claude-code-worker
  Working Dir: /Users/user/GitHub/loom/.loom/worktrees/terminal-3
  Session: loom-terminal-3
```

**Empty Case:**
```
No active terminals found. Either Loom hasn't been started yet, or all terminals have been closed.
```

**Data Source:**
- Primary: Daemon IPC (`ListTerminals` request)
- Fallback: State file (`~/.loom/state.json`) if daemon is not running

**Error Conditions:**

- **Daemon Not Running**: Falls back to reading state file
- **State File Missing**: Returns empty list
- **IPC Error**: Returns empty list with fallback attempt

**Example:**
```typescript
// List all terminals
mcp__loom-terminals__list_terminals()
```

**Use Cases:**
- Discovering available terminal IDs
- Verifying terminals were created after workspace start
- Checking terminal roles and working directories
- Finding tmux session names

---

### `get_terminal_output`

Get the recent output from a specific terminal. Returns the last N lines of output.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `terminal_id` | string | **Yes** | - | Terminal ID (e.g., `terminal-1`, `terminal-2`) |
| `lines` | number | No | 100 | Number of lines to show |

**Returns:**

Plain text containing the last N lines from the terminal's output log.

**Output Format:**
```
=== Terminal terminal-2 Output (last 100 lines) ===

[launchAgentInTerminal] START - terminalId=terminal-2...
[launchAgentInTerminal] Worktree ready at /path/.loom/worktrees/terminal-2
claude --dangerously-skip-permissions
Claude Code v1.2.3

Welcome! I'm Claude Code, ready to help with your codebase.

> _
```

**Error Conditions:**

- **Missing Parameter**: Returns error if `terminal_id` not provided
- **Terminal Not Found**: Returns message indicating terminal hasn't been created or output file doesn't exist
- **Read Error**: Returns error message with details

**Example:**
```typescript
// Read last 50 lines from terminal-2
mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-2",
  lines: 50
})

// Read default 100 lines
mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-1"
})
```

**Use Cases:**
- Checking if agent launched successfully
- Monitoring agent activity
- Debugging command execution
- Verifying terminal is responsive
- Reading agent responses

**Note:** This reads from `/tmp/loom-{terminal_id}.out` files, which are the same files accessed by `mcp-loom-logs`.

---

### `get_selected_terminal`

Get information about the currently selected (primary) terminal in Loom. Returns the terminal's ID, name, role, and recent output.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `lines` | number | No | 50 | Number of output lines to include |

**Returns:**

Plain text with terminal information and recent output.

**Output Format:**
```
=== Currently Selected Terminal ===

ID: terminal-1
Name: Architect
Role: claude-code-worker
Working Dir: /Users/user/GitHub/loom
Session: loom-terminal-1

=== Output (last 50 lines) ===

claude --dangerously-skip-permissions
Claude Code v1.2.3
Ready to assist!

> _
```

**Empty Case:**
```
No terminal is currently selected in Loom.
```

**Data Source:**
- Reads `~/.loom/state.json` to find `selectedTerminalId`
- Reads terminal output from `/tmp/loom-{terminal_id}.out`

**Error Conditions:**

- **No Selection**: Returns message if no terminal is selected
- **State File Missing**: Returns error message
- **Terminal Not Found**: Returns error if selected terminal ID not in state
- **Output Read Error**: Shows terminal info but reports error for output

**Example:**
```typescript
// Get selected terminal with default 50 lines of output
mcp__loom-terminals__get_selected_terminal()

// Get selected terminal with 100 lines of output
mcp__loom-terminals__get_selected_terminal({ lines: 100 })
```

**Use Cases:**
- Checking which terminal user is viewing
- Reading output from primary terminal
- Monitoring user's active context
- Quick access to main terminal info

---

### `send_terminal_input`

Send input (commands or text) to a specific terminal. Use this to execute commands in a terminal.

**Parameters:**

| Name | Type | Required | Default | Description |
|------|------|----------|---------|-------------|
| `terminal_id` | string | **Yes** | - | Terminal ID (e.g., `terminal-1`) |
| `input` | string | **Yes** | - | Text or command to send |

**Special Characters:**

| Character | Escape Sequence | Purpose |
|-----------|----------------|---------|
| Enter | `\n` or `\r` | Submit command |
| Ctrl+C | `\u0003` | Interrupt process |
| Ctrl+D | `\u0004` | EOF signal |
| Tab | `\t` | Tab completion |

**Returns:**

Status message indicating success or failure.

**Success Response:**
```
Input sent successfully
```

**IPC Protocol:**
- Sends `SendInput` request to daemon with `{ id: terminal_id, data: input }`
- Daemon responds with `Success` or error type
- Input is written directly to tmux session

**Error Conditions:**

- **Missing Parameters**: Returns error if `terminal_id` or `input` not provided
- **Daemon Not Running**: Returns connection error
- **Terminal Not Found**: Returns error from daemon
- **IPC Error**: Returns error message with details

**Example:**
```typescript
// Send a command with Enter
mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-1",
  input: "git status\n"
})

// Send Ctrl+C to interrupt
mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-2",
  input: "\u0003"
})

// Send multi-line input
mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-3",
  input: "cat <<EOF\nHello\nWorld\nEOF\n"
})
```

**Use Cases:**
- Executing commands in agent terminals
- Sending prompts to AI agents
- Interrupting running processes
- Testing terminal responsiveness
- Automating terminal interactions

**Warning:** Be careful with destructive commands. There is no confirmation prompt.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LOOM_SOCKET_PATH` | `/tmp/loom-daemon.sock` | Unix socket path for daemon IPC |

---

## File Locations

| File | Path | Purpose |
|------|------|---------|
| Daemon Socket | `/tmp/loom-daemon.sock` | Unix socket for IPC |
| State File | `~/.loom/state.json` | Terminal state and selection |
| Terminal Logs | `/tmp/loom-{terminal_id}.out` | Terminal output capture |

---

## Common Patterns

### Discovering Terminals and Sending Commands

```typescript
// 1. List all terminals to find IDs
const terminals = await mcp__loom-terminals__list_terminals();
// Parse output to extract terminal IDs

// 2. Send a command to a specific terminal
await mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-2",
  input: "gh issue list --state=open\n"
});

// 3. Wait a moment for command to execute
await new Promise(resolve => setTimeout(resolve, 2000));

// 4. Read terminal output to see results
const output = await mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-2",
  lines: 50
});
```

### Monitoring Agent Activity

```typescript
// 1. Get currently selected terminal
const selected = await mcp__loom-terminals__get_selected_terminal();
// Parse to find terminal ID

// 2. Periodically check output
setInterval(async () => {
  const output = await mcp__loom-terminals__get_terminal_output({
    terminal_id: selectedId,
    lines: 20
  });

  // Check for new activity, errors, or completion
}, 5000);
```

### Interactive Terminal Session

```typescript
// 1. Send initial prompt
await mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-1",
  input: "Please list all TODO comments in the codebase\n"
});

// 2. Wait for response
await new Promise(resolve => setTimeout(resolve, 3000));

// 3. Read response
const response = await mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-1",
  lines: 50
});

// 4. Send follow-up
await mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-1",
  input: "Now create a GitHub issue for the highest priority TODO\n"
});
```

### Emergency Stop

```typescript
// Stop a runaway agent or process
await mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-3",
  input: "\u0003"  // Ctrl+C
});

// Verify it stopped
const output = await mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-3",
  lines: 10
});
// Look for "^C" or command prompt return
```

### Verifying Agent Launch Success

```typescript
// 1. List terminals to check they exist
const terminals = await mcp__loom-terminals__list_terminals();
// Should show all expected terminals

// 2. Check each terminal's output for successful launch
for (const terminalId of ["terminal-2", "terminal-3", "terminal-4"]) {
  const output = await mcp__loom-terminals__get_terminal_output({
    terminal_id: terminalId,
    lines: 30
  });

  // Look for:
  // - "Claude Code" or "Codex" startup message
  // - Command prompt (">")
  // - No error messages
}
```

---

## IPC Protocol

The loom-terminals server communicates with the Loom daemon using JSON over Unix socket.

### Request Format

```json
{
  "type": "ListTerminals"
}
```

```json
{
  "type": "SendInput",
  "payload": {
    "id": "terminal-1",
    "data": "git status\n"
  }
}
```

### Response Format

**Success:**
```json
{
  "type": "Success"
}
```

**Terminal List:**
```json
{
  "type": "TerminalList",
  "payload": [
    {
      "id": "terminal-1",
      "name": "Architect",
      "role": "claude-code-worker",
      "working_dir": "/path/to/workspace",
      "tmux_session": "loom-terminal-1",
      "created_at": 1729000000
    }
  ]
}
```

**Error:**
```json
{
  "type": "Error",
  "payload": "Terminal not found: terminal-99"
}
```

---

## Error Handling

All tools return errors as plain text with appropriate context:

**Missing Parameter:**
```
Error: terminal_id is required
```

**Terminal Not Found:**
```
Terminal output file not found for terminal-5.

This usually means:
- The terminal hasn't been created yet, or
- The terminal was closed
```

**Daemon Connection Failed:**
```
Failed to connect to Loom daemon at /tmp/loom-daemon.sock: ECONNREFUSED

This usually means:
- The daemon is not running
- Wrong socket path configured
```

Common error types:
- **ENOENT**: File/terminal not found
- **ECONNREFUSED**: Daemon not running
- **ETIMEDOUT**: Daemon not responding
- **Parse Error**: Invalid daemon response

---

## Performance Considerations

- **IPC Latency**: Unix socket communication is fast (<1ms) but not instant
- **Log Reading**: Reading large terminal logs can be slow; use appropriate `lines` parameter
- **State File**: Falls back to state file if daemon unavailable (slightly slower)

---

## See Also

- [MCP Loom UI](./loom-ui.md) - UI interaction and console logs
- [MCP Loom Logs](./loom-logs.md) - Daemon and terminal log access
- [MCP Overview](./README.md) - Introduction to Loom MCP servers
- [Loom Daemon IPC Protocol](../../loom-daemon/README.md) - Low-level IPC details
