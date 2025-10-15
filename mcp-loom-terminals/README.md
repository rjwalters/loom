# MCP Loom Terminals

MCP server for interacting with Loom terminal sessions. Provides tools to list terminals, read their output, and send commands.

## Features

Provides 4 tools for interacting with Loom's terminal sessions:

1. **`list_terminals`** - List all active terminal sessions
   - Shows terminal IDs, names, roles, working directories
   - Use this to discover available terminals

2. **`get_terminal_output`** - View recent output from a specific terminal
   - Get last N lines of output (default: 100)
   - Reads from terminal log files (`/tmp/loom-*.out`)

3. **`get_selected_terminal`** - Get info about currently selected terminal
   - Shows terminal details + recent output
   - Reads from state file (`~/.loom/state.json`)

4. **`send_terminal_input`** - Send commands to a terminal
   - Execute commands in any Loom terminal
   - Use `\n` for Enter, `\u0003` for Ctrl+C

## Installation

```bash
cd mcp-loom-terminals
pnpm install
pnpm build
```

## Configuration

Add to your MCP settings (e.g., Claude Desktop config):

```json
{
  "mcpServers": {
    "loom-terminals": {
      "command": "node",
      "args": ["/Users/yourname/GitHub/loom/mcp-loom-terminals/dist/index.js"]
    }
  }
}
```

### Custom Socket Path

If you're using a custom socket path for the Loom daemon:

```json
{
  "mcpServers": {
    "loom-terminals": {
      "command": "node",
      "args": ["/Users/yourname/GitHub/loom/mcp-loom-terminals/dist/index.js"],
      "env": {
        "LOOM_SOCKET_PATH": "/path/to/custom/socket.sock"
      }
    }
  }
}
```

## Prerequisites

This MCP server requires:

1. **Loom daemon running**: The daemon must be active at `/tmp/loom-daemon.sock`
2. **State file**: Loom creates `~/.loom/state.json` with terminal info
3. **Terminal logs**: Daemon captures output to `/tmp/loom-{id}.out` (automatic)

No additional configuration needed - if Loom is running, this will work!

## Usage Examples

Once configured in Claude Desktop, you can ask:

### Discovery
- "What terminals are currently active in Loom?"
- "Show me all terminals and their roles"

### Viewing Output
- "What's the output of terminal-1?"
- "Show me the last 50 lines from the worker terminal"
- "What's showing in the currently selected terminal?"

### Sending Commands
- "Run 'git status' in terminal-2"
- "Send 'npm test' to the worker terminal"
- "Execute 'ls -la' in terminal-1"

The MCP tools will be automatically invoked to interact with the terminals.

## Development

```bash
pnpm watch  # Watch mode for development
```

## Architecture

```
┌──────────────────┐
│  Loom App        │
│  (Tauri)         │──writes──> ~/.loom/state.json
└──────────────────┘

┌──────────────────┐
│  Loom Daemon     │──manages──> Terminal Sessions (tmux)
│  (Unix Socket)   │──captures─> /tmp/loom-*.out
└──────────────────┘
           ↑
           │ (IPC via socket)
           │
┌──────────────────┐
│ MCP Loom         │──reads────> ~/.loom/state.json
│ Terminals Server │──reads────> /tmp/loom-*.out
└──────────────────┘
           ↓
           │ (provides tools)
           ↓
┌──────────────────┐
│  Claude Desktop  │
│  / Claude Code   │
└──────────────────┘
```

## How It Works

### Terminal Discovery
1. MCP server sends `ListTerminals` request to daemon via Unix socket
2. Daemon returns list of active terminal sessions
3. Falls back to reading `~/.loom/state.json` if daemon unavailable

### Output Reading
1. Each terminal's output is captured to `/tmp/loom-{id}.out`
2. MCP server reads the log file directly
3. Returns last N lines (configurable)

### Command Execution
1. MCP server sends `SendInput` request to daemon
2. Daemon uses tmux `send-keys` to inject input
3. Output appears in terminal and is captured to log file

## IPC Protocol

The daemon uses internally-tagged JSON over Unix socket:

### Request Format
```json
{
  "type": "ListTerminals"
}

{
  "type": "SendInput",
  "payload": {
    "id": "terminal-1",
    "data": "ls -la\n"
  }
}
```

### Response Format
```json
{
  "type": "TerminalList",
  "payload": [
    {
      "id": "terminal-1",
      "name": "Shell",
      "role": "default",
      "working_dir": "/Users/user/project",
      "tmux_session": "loom-terminal-1-default-1",
      "created_at": 1234567890
    }
  ]
}

{
  "type": "Success"
}
```

## Security Considerations

- **Socket permissions**: Unix socket is accessible to user only
- **Command injection**: Input is sent literally to tmux (no shell interpolation)
- **Log files**: Terminal output logs are in `/tmp` (world-readable)
- **State file**: Contains terminal metadata, no sensitive data

## Comparison with mcp-loom-logs

| Feature | mcp-loom-logs | mcp-loom-terminals |
|---------|---------------|-------------------|
| Purpose | Monitor application logs | Interact with terminals |
| Read daemon log | ✅ | ❌ |
| Read Tauri log | ✅ | ❌ |
| List terminals | ❌ | ✅ |
| Read terminal output | ✅ (via file) | ✅ (via file or daemon) |
| Send commands | ❌ | ✅ |
| Get selected terminal | ❌ | ✅ |
| Daemon communication | ❌ | ✅ |

**Use mcp-loom-logs for**: Debugging Loom itself (daemon errors, Tauri issues)

**Use mcp-loom-terminals for**: Working with the content in terminals (running commands, viewing output)

## Troubleshooting

### "Failed to connect to Loom daemon"
- Check if daemon is running: `lsof /tmp/loom-daemon.sock`
- If not running, start Loom app
- Falls back to state file for `list_terminals`

### "Terminal output file not found"
- Terminal may not have been created yet
- Terminal may have been closed (logs are deleted on close)
- Check if terminal exists: `list_terminals`

### "No terminal is currently selected"
- No terminal is focused in Loom UI
- State file may be out of date
- Try selecting a terminal in Loom first

## Future Enhancements

- **Streaming output**: Watch terminal output in real-time
- **Terminal creation**: Create new terminals via MCP
- **Terminal destruction**: Close terminals via MCP
- **Role assignment**: Change terminal roles via MCP
- **Workspace switching**: Select different workspaces
- **Terminal resizing**: Resize terminal dimensions
- **Command history**: Access terminal command history

## Notes

- Terminal output is buffered in log files (not real-time streaming yet)
- Commands are sent literally (no shell parsing or expansion)
- Multi-line commands work with `\n` separators
- Special keys: `\n` = Enter, `\u0003` = Ctrl+C, `\u0004` = Ctrl+D
- Terminal IDs are stable across app restarts
- tmux session names follow pattern: `loom-{id}-{role}-{instance}`
