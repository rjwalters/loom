# Loom MCP Servers

Loom provides three Model Context Protocol (MCP) servers that enable AI agents like Claude Code to interact with the Loom application, access logs, and control terminals programmatically.

## Overview

MCP (Model Context Protocol) is a standard protocol for connecting AI agents to external tools and data sources. Loom's MCP servers expose Loom's capabilities through a standardized interface that AI agents can use for:

- **Testing and Debugging**: Verify factory reset, monitor agent launches, check terminal state
- **Automation**: Trigger workspace operations, send commands to terminals
- **Monitoring**: Read logs, check app health, track agent activity
- **Development**: Build tools and workflows on top of Loom

## Available MCP Servers

### 1. [mcp-loom-ui](./loom-ui.md)

**Purpose**: Interact with the Loom UI, console logs, and workspace state

**Key Tools**:
- `read_console_log` - Read browser console output
- `read_state_file` - Check terminal state
- `read_config_file` - Read terminal configurations
- `get_heartbeat` - Check if app is running
- `trigger_start` - Start workspace with existing config
- `trigger_force_start` - Start without confirmation
- `trigger_factory_reset` - Reset config to defaults

**When to Use**:
- Monitoring UI state and console logs
- Triggering workspace operations
- Checking application health
- Debugging frontend issues

---

### 2. [mcp-loom-logs](./loom-logs.md)

**Purpose**: Access Loom's various log files (daemon, Tauri, terminal output)

**Key Tools**:
- `tail_daemon_log` - Read daemon logs (backend activity)
- `tail_tauri_log` - Read Tauri logs (frontend activity)
- `list_terminal_logs` - Find available terminal logs
- `tail_terminal_log` - Read specific terminal output

**When to Use**:
- Debugging daemon IPC issues
- Monitoring terminal output
- Investigating backend errors
- Verifying agent launch sequences

---

### 3. [mcp-loom-terminals](./loom-terminals.md)

**Purpose**: Interact with terminal sessions via daemon IPC and control autonomous mode

**Key Tools**:
- `list_terminals` - List active terminals with metadata
- `get_terminal_output` - Read terminal output
- `get_selected_terminal` - Get currently selected terminal
- `send_terminal_input` - Send commands to terminals
- `start_autonomous_mode` - Start interval prompts for all terminals
- `stop_autonomous_mode` - Stop all interval prompts
- `launch_interval` - Manually trigger interval prompt for a terminal

**When to Use**:
- Sending commands to agent terminals
- Monitoring agent activity in real-time
- Interactive terminal sessions
- Automating terminal workflows
- Controlling autonomous agent execution
- Testing autonomous mode behavior

---

## Quick Start

### Installation

MCP servers are automatically available when you clone the Loom repository. They're configured in `.mcp.json` and `.claude/settings.json`.

**Verify Installation**:
```bash
# Check MCP configuration
cat .mcp.json

# Verify unified package exists
ls mcp-loom/
```

### Configuration

The unified MCP server is configured in `.mcp.json`:

```json
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/Users/you/GitHub/loom"
      }
    }
  }
}
```

Or generate the configuration automatically:

```bash
./scripts/setup-mcp.sh
```

### Building MCP Server

The MCP server needs to be built before use:

```bash
cd mcp-loom && npm install && npm run build
```

### Usage from Claude Code

MCP tools are available with the `mcp__` prefix:

```typescript
// Read console logs
mcp__loom-ui__read_console_log({ lines: 50 })

// List terminals
mcp__loom-terminals__list_terminals()

// Read terminal output
mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-2",
  lines: 100
})

// Trigger factory reset
mcp__loom-ui__trigger_factory_reset()
```

---

## Common Workflows

### Testing Factory Reset

**Goal**: Verify factory reset creates all terminals and launches agents successfully

```typescript
// 1. Check app is running
const heartbeat = await mcp__loom-ui__get_heartbeat();
if (heartbeat.status !== "healthy") {
  // App needs to be started
}

// 2. Trigger factory reset
await mcp__loom-ui__trigger_factory_reset();

// 3. Force start without confirmation
await mcp__loom-ui__trigger_force_start();

// 4. Wait for terminals to be created
await new Promise(resolve => setTimeout(resolve, 5000));

// 5. Verify terminals exist
const terminals = await mcp__loom-terminals__list_terminals();
// Should show 8 terminals (terminal-1 through terminal-8)

// 6. Check each agent terminal launched successfully
for (const terminalId of ["terminal-2", "terminal-3", "terminal-4"]) {
  const output = await mcp__loom-terminals__get_terminal_output({
    terminal_id: terminalId,
    lines: 50
  });
  // Look for "Claude Code" or "Codex" startup message
}

// 7. Read console logs for any errors
const consoleLogs = await mcp__loom-ui__read_console_log({ lines: 200 });
// Check for error messages
```

### Debugging Agent Launch Failures

**Goal**: Investigate why an agent didn't start correctly

```typescript
// 1. Read console logs for launch sequence
const consoleLogs = await mcp__loom-ui__read_console_log({ lines: 100 });
// Look for [launchAgentInTerminal] messages

// 2. Check daemon logs for IPC issues
const daemonLogs = await mcp__loom-logs__tail_daemon_log({ lines: 100 });
// Look for CreateTerminal and SendInput messages

// 3. Check terminal output
const terminalOutput = await mcp__loom-terminals__get_terminal_output({
  terminal_id: "terminal-3",
  lines: 50
});
// Look for errors or stuck prompts

// 4. Check state file for worktree paths
const state = await mcp__loom-ui__read_state_file();
// Verify worktreePath is set

// 5. Check config for role settings
const config = await mcp__loom-ui__read_config_file();
// Verify roleFile and workerType are correct
```

### Monitoring Agent Activity

**Goal**: Watch what agents are doing in real-time

```typescript
// 1. List all terminals
const terminals = await mcp__loom-terminals__list_terminals();

// 2. Get current selection
const selected = await mcp__loom-terminals__get_selected_terminal();

// 3. Periodically check output
setInterval(async () => {
  const output = await mcp__loom-terminals__get_terminal_output({
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
const terminals = await mcp__loom-terminals__list_terminals();
// Parse to find "Worker 1" or desired agent

// 2. Send command
await mcp__loom-terminals__send_terminal_input({
  terminal_id: "terminal-4",
  input: "Find all TODO comments and create issues\n"
});

// 3. Wait for processing
await new Promise(resolve => setTimeout(resolve, 5000));

// 4. Read response
const output = await mcp__loom-terminals__get_terminal_output({
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
    ┌────┴─────┐
    │          │
    ▼          ▼
┌────────┐  ┌────────┐  ┌──────────┐
│ loom-ui│  │ loom-  │  │  loom-   │
│        │  │ logs   │  │terminals │
└───┬────┘  └───┬────┘  └────┬─────┘
    │           │             │
    │           │             │
    ▼           ▼             ▼
┌──────────────────────────────────┐
│      Loom Application            │
│                                  │
│  ┌──────────┐    ┌────────────┐ │
│  │  Tauri   │◄──►│   Daemon   │ │
│  │  (UI)    │    │ (Backend)  │ │
│  └──────────┘    └────────────┘ │
└──────────────────────────────────┘
         │                │
         ▼                ▼
    ~/.loom/          /tmp/
    console.log       loom-daemon.sock
    state.json        loom-*.out
    config.json
```

### File System

**Loom Directory** (`~/.loom/`):
- `console.log` - Browser console output
- `daemon.log` - Daemon activity logs
- `tauri.log` - Tauri application logs
- `mcp-command.json` - File-based IPC commands

**Workspace Directory** (`{workspace}/.loom/`):
- `state.json` - Current terminal state
- `config.json` - Terminal configurations
- `worktrees/` - Git worktrees for agents

**Temporary Directory** (`/tmp/`):
- `loom-daemon.sock` - Unix socket for IPC
- `loom-terminal-*.out` - Terminal output logs

---

## Development

### Adding New Tools

**1. Add tool to server** (`mcp-*/src/index.ts`):

```typescript
// In ListToolsRequestSchema handler
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

// In CallToolRequestSchema handler
case "my_new_tool": {
  const param1 = args?.param1 as string;
  const result = await myNewToolImpl(param1);
  return {
    content: [{ type: "text", text: result }]
  };
}
```

**2. Implement tool logic**:

```typescript
async function myNewToolImpl(param1: string): Promise<string> {
  // Tool implementation
  return "result";
}
```

**3. Document in API reference** (`docs/mcp/*.md`)

**4. Rebuild and test**:

```bash
cd mcp-loom-ui && pnpm build
# Test from Claude Code
mcp__loom-ui__my_new_tool({ param1: "test" })
```

### Testing MCP Servers

**Manual Testing**:
```bash
# Start server
node mcp-loom-ui/dist/index.js

# Send MCP protocol messages (stdin)
{"jsonrpc":"2.0","method":"tools/list","id":1}

# Should receive tool list on stdout
```

**Integration Testing** (from Claude Code):
```typescript
// Test each tool
const logs = await mcp__loom-ui__read_console_log();
const state = await mcp__loom-ui__read_state_file();
const terminals = await mcp__loom-terminals__list_terminals();
```

### Debugging

**Enable MCP Debug Logging** (in `.claude/settings.json`):
```json
{
  "mcpServers": {
    "loom-ui": {
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
- **"MCP server not found"**: Run `pnpm build` to compile TypeScript
- **"Connection refused"**: Check daemon is running (`pnpm daemon:dev`)
- **"File not found"**: Verify file paths and environment variables
- **"Parse error"**: Check JSON format in state/config files

---

## Best Practices

### Error Handling

Always check for errors before proceeding:

```typescript
const heartbeat = await mcp__loom-ui__get_heartbeat();
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

For detailed tool documentation, see individual server references:

- **[mcp-loom-ui API Reference](./loom-ui.md)** - 7 tools for UI interaction
- **[mcp-loom-logs API Reference](./loom-logs.md)** - 4 tools for log access
- **[mcp-loom-terminals API Reference](./loom-terminals.md)** - 7 tools for terminal and autonomous mode control

---

## Contributing

When adding new MCP capabilities:

1. **Add tool to appropriate server** - Choose loom-ui, loom-logs, or loom-terminals
2. **Write comprehensive documentation** - Include parameters, returns, examples, and error conditions
3. **Test thoroughly** - Verify tool works from Claude Code
4. **Update this README** - Add to tool list and workflows if applicable

---

## See Also

- [MCP Protocol Specification](https://modelcontextprotocol.io/docs)
- [Loom README](../../README.md) - Main project documentation
- [CLAUDE.md](../../CLAUDE.md) - Development context for AI agents
- [Daemon IPC Protocol](../../loom-daemon/README.md) - Low-level daemon communication
