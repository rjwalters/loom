# @loom/mcp - Unified Loom MCP Server

A unified Model Context Protocol (MCP) server that provides programmatic control over Loom for Claude Code integration.

## Installation

```bash
cd mcp-loom
npm install
npm run build
```

## Configuration

### User-scope registration (primary path, #4230)

The `loom` server is registered **once per machine at user scope**, pointing at
the machine-level checkout's bundle — one instance then serves **every** repo:

```bash
claude mcp add --scope user loom -- node ~/.local/share/loom/mcp-loom/dist/index.js
```

`scripts/install-loom.sh` does this at install time (idempotently), and
`loom update` refreshes the served bundle for every repo at once (the #3803
stale-dist drift fix). You normally do **not** hand-edit a per-repo `.mcp.json`
for `loom` anymore — a project-scope `loom` entry would *shadow* the user-scope
server (Claude Code precedence is local > project > user), so `setup-mcp.sh`
strips any legacy project entry it finds. See
[`../defaults/docs/machine-dispatcher.md`](../defaults/docs/machine-dispatcher.md)
for the full registration / migration / shadowing story.

### Workspace discovery (how one server serves many repos)

Because a single user-scope instance carries no per-repo env, it resolves the
**invoking** repo from its process CWD (`getWorkspacePath()` in
`src/shared/config.ts`):

1. `LOOM_WORKSPACE` env override — explicit, highest precedence.
2. Walk up from `process.cwd()` to a repo root (`.loom/` or `.git` marker). A
   **linked worktree** CWD (`.git` is a *file*) resolves to the **main
   checkout** via the git common dir, mirroring `resolve_mcp_workspace()` in
   `defaults/scripts/claude-wrapper.sh`, so `.loom/config.json` is read from the
   right place.
3. **Loud failure** (throws) when neither is found — there is deliberately **no**
   silent `~/GitHub/loom` fallback, which under user scope would silently operate
   on the wrong repo.

This relies on Claude Code launching stdio MCP servers with cwd = the session's
project directory.

### Manual / single-repo `.mcp.json` (legacy)

You can still register per-repo if needed (e.g. Claude Desktop), but set
`LOOM_WORKSPACE` explicitly so discovery is bypassed:

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
| `LOOM_WORKSPACE` | Path to the Loom workspace (bypasses CWD discovery) | CWD-based repo-root discovery; **no** `~/GitHub/loom` fallback |
| `LOOM_SOCKET_PATH` | Path to daemon socket | `~/.loom/loom-daemon.sock` |

## Available Tools (30 total)

The MCP server provides 30 tools organized by function.

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

### Durable Watches (3 tools)

These tools register durable operator watches on issue/PR terminal state (#3971).
A watch is persisted machine-level by the long-lived `loom-daemon`, so it survives
the registering session's death **and** a daemon restart; resolutions are appended
to `~/.loom/logs/watch-results.log`.

| Tool | Description |
|------|-------------|
| `register_watch` | Register a durable watch on an issue/PR (cross-repo via `repo`/`workspace_root`; idempotent) |
| `list_watches` | List active durable watches |
| `remove_watch` | Remove a watch by id |

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

   Under user-scope registration (#4230) the safe one-shot refresh is **`loom
   update`**, which rebuilds this bundle when `dist/index.js` is **missing or
   older than any file under `src/`** and serves it to every repo at once.
   (`./scripts/setup-mcp.sh` still rebuilds the bundle too, but it is demoted —
   it no longer registers `loom`; see the Configuration section.)

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
