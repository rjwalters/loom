# @loom/mcp - Unified Loom MCP Server

A unified Model Context Protocol (MCP) server that provides programmatic control over Loom for Claude Code integration.

This package consolidates three previously separate MCP servers:
- `mcp-loom-logs` - Log monitoring tools
- `mcp-loom-ui` - UI control and state management tools
- `mcp-loom-terminals` - Terminal management tools

## Installation

```bash
cd mcp-loom
npm install
npm run build
```

## Configuration

Add to your MCP settings (`.mcp.json` or Claude Desktop config):

```json
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["/path/to/loom/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/path/to/your/workspace"
      }
    }
  }
}
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `LOOM_WORKSPACE` | Path to the Loom workspace | `~/GitHub/loom` |
| `LOOM_SOCKET_PATH` | Path to daemon socket | `~/.loom/loom-daemon.sock` |

## Available Tools

### Log Tools

| Tool | Description |
|------|-------------|
| `tail_daemon_log` | Tail the Loom daemon log file (~/.loom/daemon.log) |
| `tail_tauri_log` | Tail the Tauri application log file (~/.loom/tauri.log) |
| `list_terminal_logs` | List all available terminal output logs |
| `tail_terminal_log` | Tail a specific terminal's output log |

### UI Tools

| Tool | Description |
|------|-------------|
| `read_console_log` | Read browser console log for debugging |
| `trigger_start` | Start engine with confirmation dialog |
| `trigger_force_start` | Start engine without confirmation |
| `trigger_factory_reset` | Reset workspace with confirmation |
| `trigger_force_factory_reset` | Reset workspace without confirmation |
| `read_state_file` | Read workspace state file |
| `read_config_file` | Read workspace config file |
| `get_heartbeat` | Check if Loom app is running |
| `trigger_restart_terminal` | Restart a specific terminal |
| `stop_engine` | Stop all terminals |
| `trigger_run_now` | Execute interval prompt immediately |
| `get_ui_state` | Get comprehensive UI state |
| `get_random_file` | Get random file from workspace |

### Terminal Tools

| Tool | Description |
|------|-------------|
| `list_terminals` | List all active terminal sessions |
| `get_terminal_output` | Get recent output from a terminal |
| `get_selected_terminal` | Get info about selected terminal |
| `send_terminal_input` | Send input to a terminal |
| `check_tmux_server_health` | Check tmux server status |
| `get_tmux_server_info` | Get tmux server details |
| `toggle_tmux_verbose_logging` | Enable tmux debug logging |
| `create_terminal` | Create a new terminal session |
| `delete_terminal` | Delete a terminal session |
| `restart_terminal` | Restart a terminal preserving config |
| `configure_terminal` | Update terminal settings |
| `set_primary_terminal` | Set primary terminal in UI |
| `clear_terminal_history` | Clear terminal scrollback |
| `start_autonomous_mode` | Start autonomous mode |
| `stop_autonomous_mode` | Stop autonomous mode |
| `launch_interval` | Trigger interval prompt manually |
| `get_agent_metrics` | Get agent performance metrics |

## Example Usage

```typescript
// Via Claude Code MCP integration
const terminals = await mcp__loom__list_terminals();
const state = await mcp__loom__get_ui_state();
const logs = await mcp__loom__tail_daemon_log({ lines: 50 });

// Create and configure a terminal
await mcp__loom__create_terminal({ name: "Builder", role: "builder" });
await mcp__loom__configure_terminal({
  terminal_id: "terminal-1",
  role_config: { targetInterval: 300000 }
});
```

## Migration from Separate Packages

If you were using the separate `mcp-loom-logs`, `mcp-loom-ui`, or `mcp-loom-terminals` packages, update your configuration:

**Before (multiple servers):**
```json
{
  "mcpServers": {
    "loom-logs": { "command": "node", "args": ["mcp-loom-logs/dist/index.js"] },
    "loom-ui": { "command": "node", "args": ["mcp-loom-ui/dist/index.js"] },
    "loom-terminals": { "command": "node", "args": ["mcp-loom-terminals/dist/index.js"] }
  }
}
```

**After (unified server):**
```json
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["mcp-loom/dist/index.js"],
      "env": { "LOOM_WORKSPACE": "/path/to/workspace" }
    }
  }
}
```

All tool names remain the same, so no code changes are needed for tool calls.

## Development

```bash
# Watch mode for development
npm run watch

# Build for production
npm run build
```

## Architecture

```
mcp-loom/
├── src/
│   ├── index.ts           # Main server entry point
│   ├── types.ts           # Shared TypeScript types
│   ├── shared/
│   │   ├── config.ts      # Workspace/state file utilities
│   │   ├── ipc.ts         # File-based IPC with retry
│   │   ├── daemon.ts      # Socket-based daemon communication
│   │   └── formatting.ts  # Log/output formatting
│   └── tools/
│       ├── logs.ts        # Log tools
│       ├── ui.ts          # UI tools
│       └── terminals.ts   # Terminal tools
├── package.json
└── tsconfig.json
```

## License

MIT
