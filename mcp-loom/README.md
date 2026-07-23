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

## Available Tools (27 total)

The MCP server provides 27 tools organized by function.

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

### Sweep Dispatch (8 tools)

These tools front the Rust `loom-daemon` (Tier 2) over its Unix-socket IPC and back
`/loom:sweep`'s Stage -1 backend detection. They require a running `loom-daemon`.

| Tool | Description |
|------|-------------|
| `dispatch_sweep` | Dispatch a `/loom:sweep <N>` for an issue (multi-account token rotation) |
| `list_sweeps` | Enumerate running sweeps in the daemon registry |
| `get_sweep_status` | Inspect a running sweep's state |
| `tail_sweep_log` | Tail a per-sweep log file |
| `cancel_sweep` | Cancel a running sweep (SIGTERM → grace → SIGKILL) |
| `publish_event` | Publish a sweep-lifecycle event on the event bus |
| `subscribe_to_events` | Stream topic-filtered events to a subscriber |
| `tail_event_bus` | Tail the event bus without subscribing to a topic |

> **If these tools are missing from a live session**, `dist/index.js` is almost
> certainly a **stale build** predating the sweep tools. See
> [Rebuilding after source changes](#rebuilding-after-source-changes-reconnect-required).

## Removed Tools

The following tools were removed to reduce complexity. Use the alternatives listed:

| Removed Tool | Alternative |
|--------------|-------------|
| `tail_daemon_log` | `tail -n 20 ~/.loom/daemon.log` |
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

## Rebuilding after source changes (reconnect required)

The MCP client (Claude Code, Claude Desktop) loads the **built bundle** at
`dist/index.js` — never the TypeScript source. Two things follow:

1. **`dist/index.js` can silently drift behind `src/`.** If you add or change a
   tool in `src/tools/*.ts` but never rebuild, the running server keeps exposing
   the old tool list. This is exactly how the sweep-dispatch tools went missing
   from live sessions (#3803): `dist/index.js` was a months-old build predating
   `src/tools/sweeps.ts`. Always rebuild after touching source:

   ```bash
   cd mcp-loom && npm run build     # tsc --noEmit && rm -rf dist && node esbuild.config.js
   ```

   `scripts/setup-mcp.sh` now rebuilds automatically when `dist/index.js` is
   **missing or older than any file under `src/`**, so `./scripts/setup-mcp.sh`
   is the safe one-shot path.

2. **Rebuilding on disk does NOT refresh an already-running session.** An MCP
   client caches the tool list from its stdio-spawned child process **at connect
   time**. Overwriting `dist/index.js` while a session is live changes nothing
   until the client reconnects. After rebuilding you must **restart the Claude
   Code session** (or otherwise respawn the `loom` MCP server subprocess) for the
   new tools to appear.

**Verify a rebuild picked up a tool** without a full session restart:

```bash
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | node mcp-loom/dist/index.js 2>/dev/null | grep -o '"dispatch_sweep"'
```

A non-empty match means the bundle exposes the tool; if a live session still
can't see it, the session needs to reconnect (point 2).

See also [`.loom/docs/troubleshooting.md`](../.loom/docs/troubleshooting.md) →
"Sweep MCP tools missing (stale dist bundle)".

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
│       ├── terminals.ts   # Terminal management tools
│       └── sweeps.ts      # Sweep-dispatch tools (loom-daemon IPC)
├── package.json
└── tsconfig.json
```

## License

MIT
