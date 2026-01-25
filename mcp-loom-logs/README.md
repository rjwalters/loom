# MCP Loom Logs

> **DEPRECATED**: This package has been consolidated into the unified `mcp-loom` package.
> Please use `mcp-loom` instead. See [mcp-loom/README.md](../mcp-loom/README.md) for migration instructions.

MCP server for monitoring Loom application logs in real-time.

## Features

Provides 4 tools for accessing Loom's logs:

1. **`tail_daemon_log`** - View recent daemon logs (`~/.loom/daemon.log`)
   - Shows terminal creation, IPC requests, and daemon errors

2. **`tail_tauri_log`** - View recent Tauri app logs (`~/.loom/tauri.log`)
   - Shows frontend activity, state changes, and UI errors

3. **`list_terminal_logs`** - List all terminal output logs
   - Shows available `/tmp/loom-*.out` files

4. **`tail_terminal_log`** - View a specific terminal's output
   - Pass `terminal_id` like "terminal-1", "terminal-2", etc.

## Installation

```bash
cd mcp-loom-logs
pnpm install
pnpm build
```

## Configuration

Add to your MCP settings (e.g., Claude Desktop config):

```json
{
  "mcpServers": {
    "loom-logs": {
      "command": "node",
      "args": ["/Users/yourname/GitHub/loom/mcp-loom-logs/dist/index.js"]
    }
  }
}
```

## Prerequisites

For this MCP server to work, Loom needs to be configured to write logs to files:

### Daemon Logging

The daemon needs to write to `~/.loom/daemon.log`. This can be configured via environment variable or startup script.

**Option 1: Environment variable**
```bash
export LOOM_DAEMON_LOG=~/.loom/daemon.log
```

**Option 2: Redirect in startup script** (in `scripts/start-daemon.sh`):
```bash
./loom-daemon 2>&1 | tee ~/.loom/daemon.log
```

### Tauri Logging

The Tauri app needs to write console logs to `~/.loom/tauri.log`.

**In src/main.ts, add at the very beginning:**
```typescript
// Redirect console logs to file
const logPath = join(homeDir(), '.loom/tauri.log');
const logStream = fs.createWriteStream(logPath, { flags: 'a' });
const originalLog = console.log;
const originalError = console.error;
const originalWarn = console.warn;

console.log = (...args) => {
  const message = args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' ');
  logStream.write(`[LOG] ${new Date().toISOString()} ${message}\n`);
  originalLog(...args);
};

console.error = (...args) => {
  const message = args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' ');
  logStream.write(`[ERROR] ${new Date().toISOString()} ${message}\n`);
  originalError(...args);
};

console.warn = (...args) => {
  const message = args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' ');
  logStream.write(`[WARN] ${new Date().toISOString()} ${message}\n`);
  originalWarn(...args);
};
```

### Terminal Output Logs

Terminal output logs already work! The daemon captures all terminal output to `/tmp/loom-{id}.out` files automatically.

## Usage Examples

Once configured in Claude Desktop, you can ask:

- "Show me the last 50 lines of the daemon log"
- "What terminals are currently logging?"
- "Show me the output from terminal-1"
- "Are there any errors in the Tauri log?"

The MCP tools will be automatically invoked to fetch the logs.

## Development

```bash
pnpm watch  # Watch mode for development
```

## Architecture

```
┌─────────────┐
│  Loom App   │
│  (Tauri)    │──writes──> ~/.loom/tauri.log
└─────────────┘

┌─────────────┐
│ Loom Daemon │──writes──> ~/.loom/daemon.log
└─────────────┘

┌─────────────┐
│  Terminal 1 │──captures─> /tmp/loom-terminal-1.out
│  Terminal 2 │──captures─> /tmp/loom-terminal-2.out
│  Terminal N │──captures─> /tmp/loom-terminal-N.out
└─────────────┘

           ↓ (reads)

┌─────────────────┐
│ MCP Loom Logs   │
│ Server          │
└─────────────────┘

           ↓ (provides tools)

┌─────────────────┐
│ Claude Desktop  │
│ / Claude Code   │
└─────────────────┘
```

## Notes

- Log files are appended to, so they can grow over time
- Consider adding log rotation if needed
- Terminal output logs are automatically cleaned up when terminals are destroyed
- The daemon log is most useful for debugging terminal creation issues
- The Tauri log is most useful for debugging UI and state management issues
