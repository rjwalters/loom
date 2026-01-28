# @loom/mcp - Unified Loom MCP Server

A unified Model Context Protocol (MCP) server that provides programmatic control over Loom for Claude Code integration.

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

## Available Tools (19 total)

The MCP server provides 19 essential tools organized by function.

### UI/Engine Control (6 tools)

| Tool | Description |
|------|-------------|
| `trigger_start` | Start engine with confirmation dialog |
| `trigger_force_start` | Start engine without confirmation |
| `trigger_force_factory_reset` | Reset workspace without confirmation |
| `get_heartbeat` | Check if Loom app is running |
| `stop_engine` | Stop all terminals |
| `get_ui_state` | Get comprehensive UI state (workspace, config, terminals) |

### Terminal Management (13 tools)

| Tool | Description |
|------|-------------|
| `list_terminals` | List all active terminal sessions |
| `get_terminal_output` | Get recent output from a terminal |
| `get_selected_terminal` | Get info about selected terminal |
| `send_terminal_input` | Send input to a terminal |
| `create_terminal` | Create a new terminal session |
| `delete_terminal` | Delete a terminal session |
| `restart_terminal` | Restart a terminal preserving config |
| `configure_terminal` | Update terminal settings |
| `set_primary_terminal` | Set primary terminal in UI |
| `start_autonomous_mode` | Start autonomous mode |
| `stop_autonomous_mode` | Stop autonomous mode |
| `launch_interval` | Trigger interval prompt manually |
| `get_agent_metrics` | Get agent performance metrics |

## Removed Tools

The following tools were removed to reduce complexity. Use the alternatives listed:

| Removed Tool | Alternative |
|--------------|-------------|
| `tail_daemon_log` | `tail -n 20 ~/.loom/daemon.log` |
| `tail_tauri_log` | `tail -n 20 ~/.loom/tauri.log` |
| `list_terminal_logs` | `ls /tmp/loom-*.out` |
| `tail_terminal_log` | `tail -n 20 /tmp/loom-terminal-1.out` |
| `read_console_log` | `tail -n 20 ~/.loom/console.log` |
| `read_state_file` | Use `get_ui_state` (provides state + context) |
| `read_config_file` | Use `get_ui_state` (provides config + context) |
| `trigger_factory_reset` | Use `trigger_force_factory_reset` |
| `trigger_restart_terminal` | Use `restart_terminal` |
| `trigger_run_now` | Use `launch_interval` |
| `get_random_file` | Use `.loom/scripts/random-file.sh` |
| `check_tmux_server_health` | tmux debugging - use bash directly |
| `get_tmux_server_info` | tmux debugging - use bash directly |
| `toggle_tmux_verbose_logging` | tmux debugging - send SIGUSR2 manually |
| `clear_terminal_history` | Use `restart_terminal` instead |

## Example Usage

```typescript
// Via Claude Code MCP integration
const terminals = await mcp__loom__list_terminals();
const state = await mcp__loom__get_ui_state();

// Create and configure a terminal
await mcp__loom__create_terminal({ name: "Builder", role: "builder" });
await mcp__loom__configure_terminal({
  terminal_id: "terminal-1",
  role_config: { targetInterval: 300000 }
});

// Trigger autonomous work
await mcp__loom__launch_interval({ terminal_id: "terminal-1" });
```

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
│       ├── logs.ts        # Log tools (empty - use bash)
│       ├── ui.ts          # UI/Engine tools
│       └── terminals.ts   # Terminal management tools
├── package.json
└── tsconfig.json
```

## License

MIT
