# Loom MCP Server

Loom provides a unified Model Context Protocol (MCP) server (`mcp-loom`) that enables AI agents like Claude Code to interact with the Loom application, dispatch sweeps on the Rust `loom-daemon`, and control terminals programmatically.

## Overview

MCP (Model Context Protocol) is a standard protocol for connecting AI agents to external tools and data sources. Loom's MCP server exposes Loom's capabilities through a standardized interface that AI agents can use for:

- **Orchestration**: Dispatch and monitor `/loom:sweep` runs on the Tier 2 daemon
- **Automation**: Trigger workspace operations, send commands to terminals
- **Monitoring**: Stream sweep-lifecycle events, register durable watches, check app health
- **Development**: Build tools and workflows on top of Loom

## Available Tools

The unified `mcp-loom` package provides all 30 Loom MCP tools in a single server,
organized into four categories. The authoritative per-tool tables live in
[`mcp-loom/README.md`](../../mcp-loom/README.md#available-tools-30-total) — the
summary below is an orientation map, not a second source of truth.

### UI/Engine Control (6 tools)

**Purpose**: Start/stop the engine and inspect app-level state

`trigger_start`, `trigger_force_start`, `trigger_force_factory_reset`,
`get_heartbeat`, `stop_engine`, `get_ui_state`

**When to Use**:
- Checking application health (`get_heartbeat`)
- Starting or stopping the engine, with or without confirmation
- Reading comprehensive workspace/config/terminal state (`get_ui_state`)

---

### Terminal Management (13 tools)

**Purpose**: Interact with terminal sessions via daemon IPC and control autonomous mode

`list_terminals`, `get_terminal_output`, `get_selected_terminal`,
`send_terminal_input`, `create_terminal`, `delete_terminal`, `restart_terminal`,
`configure_terminal`, `set_primary_terminal`, `start_autonomous_mode`,
`stop_autonomous_mode`, `launch_interval`, `get_agent_metrics`

**When to Use**:
- Sending commands to agent terminals
- Monitoring agent activity in real-time
- Automating terminal workflows
- Controlling autonomous agent execution

See the [Terminal Tools Reference](./loom-terminals.md) for detailed parameters.

---

### Sweep Dispatch (8 tools)

**Purpose**: Drive the Rust `loom-daemon` (Tier 2) over its Unix-socket IPC —
dispatch single-issue sweeps and observe the event bus. Requires a running
`loom-daemon`.

`dispatch_sweep`, `list_sweeps`, `get_sweep_status`, `tail_sweep_log`,
`cancel_sweep`, `publish_event`, `subscribe_to_events`, `tail_event_bus`

**When to Use**:
- Dispatching `/loom:sweep <N>` with multi-account token rotation
- Inspecting or cancelling in-flight sweeps
- Streaming sweep-lifecycle events

---

### Durable Watches (3 tools)

**Purpose**: Register machine-level watches on issue/PR terminal state (#3971)
that survive the registering session's death and daemon restarts. Resolutions
append to `~/.loom/logs/watch-results.log`.

`register_watch`, `list_watches`, `remove_watch`

---

## Quick Start

### Installation

Since #4230, the `loom` MCP server is registered **once per machine at user
scope**, not per-repo. `scripts/install-loom.sh` does this at install time
(idempotently), and `loom update` refreshes the registration afterward — no
per-repo `.mcp.json` is needed or generated.

**Verify Installation**:
```bash
# Check the user-scope registration
claude mcp list --scope user

# Verify unified package exists
ls mcp-loom/
```

### Configuration

Registration is a one-time, machine-level `claude mcp add`:

```bash
claude mcp add --scope user loom -- node ~/.local/share/loom/mcp-loom/dist/index.js
```

A single user-scoped instance serves every repo: mcp-loom resolves the
**invoking** repo from its process CWD (`LOOM_WORKSPACE` env override, then a
walk up from `process.cwd()` to a `.loom/`/`.git` root). See
[`defaults/docs/machine-dispatcher.md`](../../defaults/docs/machine-dispatcher.md)
for the full registration model and why per-repo `.mcp.json` generation was
demoted (project-scope entries outrank and silently shadow the user-scope
server). `scripts/setup-mcp.sh` is now only a bundle-rebuild/legacy-migration
tool, with a safehouse-only residual role emitting a per-repo `.mcp.json`
containing just the `safehouse` server.

### Building MCP Server

The MCP server needs to be built before use:

```bash
cd mcp-loom && npm install && npm run build
```

### Usage from Claude Code

MCP tools are available with the `mcp__loom__` prefix:

```typescript
// List terminals
mcp__loom__list_terminals()

// Read terminal output
mcp__loom__get_terminal_output({
  terminal_id: "terminal-2",
  lines: 100
})

// Dispatch a sweep for issue #123
mcp__loom__dispatch_sweep({ kind: { Issue: 123 } })

// Check on it
mcp__loom__get_sweep_status({ sweep_id: "sweep-..." })
```

---

## Common Workflows

### Dispatching and Monitoring a Sweep

**Goal**: Run a full issue lifecycle on the daemon and watch it complete

```typescript
// 1. Check the daemon is reachable and see what's already running
const sweeps = await mcp__loom__list_sweeps();

// 2. Dispatch the issue
const dispatch = await mcp__loom__dispatch_sweep({ kind: { Issue: 123 } });

// 3. Poll status (or subscribe to events instead)
const status = await mcp__loom__get_sweep_status({ sweep_id: dispatch.sweep_id });

// 4. Tail the per-sweep log if something looks stuck
const log = await mcp__loom__tail_sweep_log({ sweep_id: dispatch.sweep_id, lines: 50 });

// 5. Cancel if needed (SIGTERM → grace → SIGKILL)
await mcp__loom__cancel_sweep({ sweep_id: dispatch.sweep_id });
```

### Watching an Issue/PR to Resolution

**Goal**: Get durable notification when an issue or PR reaches terminal state,
even if this session dies

```typescript
// Register the watch (idempotent; cross-repo via repo/workspace_root)
await mcp__loom__register_watch({ kind: "pr", number: 456 });

// List active watches
const watches = await mcp__loom__list_watches();

// Resolutions land in ~/.loom/logs/watch-results.log
```

### Monitoring Agent Activity

**Goal**: Watch what agents are doing in real-time

```typescript
// 1. List all terminals
const terminals = await mcp__loom__list_terminals();

// 2. Get current selection
const selected = await mcp__loom__get_selected_terminal();

// 3. Periodically check output
setInterval(async () => {
  const output = await mcp__loom__get_terminal_output({
    terminal_id: "terminal-2",
    lines: 20
  });

  // Parse output for agent activity
}, 10000);  // Every 10 seconds
```

### Sending Commands to Agents

**Goal**: Manually trigger agent actions or test terminal input

```typescript
// 1. Find terminal ID
const terminals = await mcp__loom__list_terminals();
// Parse to find "Worker 1" or desired agent

// 2. Send command
await mcp__loom__send_terminal_input({
  terminal_id: "terminal-4",
  input: "Find all TODO comments and create issues\n"
});

// 3. Wait for processing
await new Promise(resolve => setTimeout(resolve, 5000));

// 4. Read response
const output = await mcp__loom__get_terminal_output({
  terminal_id: "terminal-4",
  lines: 50
});
```

---

## Architecture

### Data Flow

```
┌─────────────────┐
│   AI Agent      │  (Claude Code)
│  (Your MCP)     │
└────────┬────────┘
         │
         │ MCP Protocol (stdio)
         │
         ▼
┌──────────────────┐
│     mcp-loom     │  (Unified MCP Server)
│                  │
│  ┌────────────┐  │
│  │ UI/Engine  │  │
│  ├────────────┤  │
│  │ Terminals  │  │
│  ├────────────┤  │
│  │  Sweeps    │  │
│  ├────────────┤  │
│  │  Watches   │  │
│  └────────────┘  │
└────────┬─────────┘
         │ Unix-socket IPC
         ▼
┌──────────────────────────────────┐
│           loom-daemon            │
│  Orchestration + IPC server      │
└──────────────────────────────────┘
```

### File System

**Machine-level Loom Directory** (`~/.loom/`):
- `loom-daemon.sock` - Unix socket for daemon IPC (override with `LOOM_SOCKET_PATH`)
- `daemon.log` - Daemon activity logs
- `watches.json` - Durable watch registry
- `logs/watch-results.log` - Terminal resolutions of watches

**Workspace Directory** (`{workspace}/.loom/`):
- `config.json` - Terminal/role configurations
- `worktrees/` - Git worktrees for agents

---

## Development

### Adding New Tools

**1. Add tool to the unified server** (`mcp-loom/src/tools/*.ts`):

Choose the appropriate file based on tool category:
- `ui.ts` - UI/engine control and state tools
- `terminals.ts` - Terminal management tools
- `sweeps.ts` - Sweep dispatch, event bus, and durable watch tools

```typescript
// In the appropriate tools file, add to the tools array
{
  name: "my_new_tool",
  description: "What this tool does",
  inputSchema: {
    type: "object",
    properties: {
      param1: {
        type: "string",
        description: "Parameter description"
      }
    },
    required: ["param1"]
  }
}

// Add handler in the handlers object
my_new_tool: async (args) => {
  const param1 = args?.param1 as string;
  const result = await myNewToolImpl(param1);
  return { content: [{ type: "text", text: result }] };
}
```

**2. Implement tool logic**:

```typescript
async function myNewToolImpl(param1: string): Promise<string> {
  // Tool implementation
  return "result";
}
```

**3. Document it** — update the tool table in `mcp-loom/README.md` (the
authoritative list) and, for terminal tools, `docs/mcp/loom-terminals.md`

**4. Rebuild and test**:

```bash
cd mcp-loom && npm run build
# Test from Claude Code
mcp__loom__my_new_tool({ param1: "test" })
```

### Testing MCP Server

**Manual Testing**:
```bash
# Start server
node mcp-loom/dist/index.js

# Send MCP protocol messages (stdin)
{"jsonrpc":"2.0","method":"tools/list","id":1}

# Should receive tool list on stdout
```

**Integration Testing** (from Claude Code):
```typescript
// Test tools from unified server
const heartbeat = await mcp__loom__get_heartbeat();
const terminals = await mcp__loom__list_terminals();
const sweeps = await mcp__loom__list_sweeps();
```

### Debugging

**Enable MCP Debug Logging** (in `.claude/settings.json`):
```json
{
  "mcpServers": {
    "loom": {
      "debug": true
    }
  }
}
```

**Check MCP Server Logs**:
```bash
# MCP servers write to stderr
tail -f ~/.claude/mcp-*.log
```

**Common Issues**:
- **"MCP server not found"**: Run `npm run build` in mcp-loom/ to compile TypeScript
- **"Connection refused"**: Check daemon is running (`pnpm daemon:dev`)
- **"File not found"**: Verify file paths and environment variables
- **"Parse error"**: Check JSON format in state/config files

---

## Best Practices

### Error Handling

Always check for errors before proceeding:

```typescript
const heartbeat = await mcp__loom__get_heartbeat();
if (heartbeat.status === "not_running") {
  throw new Error("Loom app is not running");
}
```

### Performance

- **Use appropriate `lines` parameters** - Don't read entire logs if you only need recent entries
- **Batch operations** - Group related MCP calls together
- **Cache results** - Avoid repeated calls for static data (like terminal IDs)

### Security

- **Be careful with `send_terminal_input`** - No confirmation for destructive commands
- **Validate user input** - Always validate before sending to MCP tools
- **Limit permissions** - MCP servers have full filesystem access via Node.js

---

## API Reference

For detailed tool documentation, see:

- **[`mcp-loom/README.md`](../../mcp-loom/README.md)** - authoritative table of all 30 tools
- **[Terminal Tools Reference](./loom-terminals.md)** - detailed parameters for terminal and autonomous-mode tools

---

## Contributing

When adding new MCP capabilities:

1. **Add tool to mcp-loom package** - All tools go in the unified `mcp-loom/src/tools/` directory
2. **Choose the right category** - Add to `ui.ts`, `terminals.ts`, or `sweeps.ts`
3. **Write comprehensive documentation** - Include parameters, returns, examples, and error conditions
4. **Test thoroughly** - Verify tool works from Claude Code
5. **Update the tool tables** - `mcp-loom/README.md` first; this overview only if a category changes

---

## See Also

- [MCP Protocol Specification](https://modelcontextprotocol.io/docs)
- [Loom README](../../README.md) - Main project documentation
- [CLAUDE.md](../../CLAUDE.md) - Development context for AI agents
- [Daemon IPC Protocol](../../.loom/docs/daemon-reference.md) - Low-level daemon communication
