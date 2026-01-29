# Interactive Daemon Development Mode

## Overview

The `pnpm run daemon:dev` command provides an **interactive monitoring dashboard** for the Loom daemon, making development more productive and debugging easier.

## Visual Example

```
╔════════════════════════════════════════╗
║     Loom Daemon - Development Mode    ║
╚════════════════════════════════════════╝

🚀 Starting daemon...
✓ Daemon started (PID: 12345)
  Socket: /Users/you/.loom/daemon.sock
  Logs: .daemon.log

════════════════════════════════════════
Live Activity Monitor
Press Ctrl+C to stop daemon and exit
════════════════════════════════════════

Status: ● Running  Uptime: 00:05:23  Terminals: 6  Connections: 2

[2025-10-13T10:30:15Z INFO  loom_daemon] Restored 6 terminals
[2025-10-13T10:30:15Z INFO  loom_daemon] Loom daemon starting...
[2025-10-13T10:30:15Z INFO  loom_daemon::ipc] IPC server listening...
[2025-10-13T10:30:20Z INFO  loom_daemon::ipc] Client connected
[2025-10-13T10:30:22Z INFO  loom_daemon] CreateTerminal: Worker 1
[2025-10-13T10:30:25Z WARN  loom_daemon] Terminal abc123 output buffer full
[2025-10-13T10:30:30Z ERROR loom_daemon] Failed to attach to session xyz789
```

## Features

### 🎨 Color-Coded Logs

| Log Level | Color | Example |
|-----------|-------|---------|
| ERROR | Red | Failed operations, crashes |
| WARN | Yellow | Buffer issues, deprecations |
| INFO (connections) | Cyan | Client connected/disconnected |
| INFO (terminals) | Green | Terminal created/destroyed |
| INFO (startup) | Green | Daemon initialization |
| Polling (SendInput/GetOutput) | Gray | High-frequency messages |

### 📊 Real-Time Metrics

**Status Bar** (updates every line):
- **Status**: ● Running / ● Stopped
- **Uptime**: HH:MM:SS counter
- **Terminals**: Count of restored terminals
- **Connections**: Active client connections
- **Errors**: Recent error count (last 50 lines)
- **Warnings**: Recent warning count (last 50 lines)

### 🔍 Smart Log Filtering

The script automatically highlights important events:

```typescript
// High priority (bright colors)
- ERROR: All error messages
- WARN: All warnings
- Restored X terminals: Daemon startup
- IPC server listening: Ready to accept connections
- CreateTerminal / DestroyTerminal: Terminal lifecycle

// Medium priority (cyan)
- Client connected: New IPC connection
- Client disconnected: IPC cleanup

// Low priority (gray)
- SendInput / GetTerminalOutput: Polling traffic (noisy)
```

### 🛠 Development Benefits

1. **Immediate Feedback**: See errors as they happen
2. **Connection Monitoring**: Know when frontend connects/disconnects
3. **Performance Insight**: Watch for buffer warnings or slow operations
4. **Easy Debugging**: Color-coded output helps spot issues quickly
5. **Clean Shutdown**: Ctrl+C stops daemon gracefully

## Usage

### Two-Terminal Workflow (Recommended)

**Terminal 1:**
```bash
pnpm run daemon:dev
```

**Terminal 2:**
```bash
pnpm run tauri:dev
```

**What you'll see:**

1. **Terminal 1** shows daemon activity:
   - Daemon startup sequence
   - Client connection when Tauri app launches
   - Terminal creation/destruction
   - IPC request stream
   - Errors and warnings in real-time

2. **Terminal 2** shows Tauri/Vite output:
   - Hot reload events
   - Build status
   - Browser console logs

### When to Use Each Mode

| Mode | Command | Use Case |
|------|---------|----------|
| **Interactive Dev** | `pnpm run daemon:dev` | Daily development (recommended) |
| **Background** | `pnpm run app:dev` | Automated workflows, CI testing |
| **Foreground Cargo** | `pnpm run daemon:run` | Low-level Rust debugging with breakpoints |

## Technical Details

### Script Location
`scripts/dev-daemon.sh`

### What It Does

1. **Startup**:
   - Checks for existing daemon (stops if found)
   - Calls `scripts/start-daemon.sh` to launch daemon
   - Verifies daemon is running
   - Shows status information

2. **Monitoring Loop**:
   - Tails `.daemon.log` file
   - Colors each line based on content
   - Updates metrics periodically
   - Counts errors/warnings in recent history

3. **Cleanup**:
   - Traps `INT` and `TERM` signals (Ctrl+C)
   - Calls `scripts/stop-daemon.sh`
   - Kills tail process
   - Exits cleanly

### Files Used

- `.daemon.pid` - Process ID of running daemon
- `.daemon.log` - Full daemon output log
- `~/.loom/daemon.sock` - Unix socket for IPC

All files are gitignored.

### Monitoring Implementation

**Connection Count**:
```bash
lsof ~/.loom/daemon.sock | grep -v COMMAND | wc -l
```

**Terminal Count**:
```bash
grep -i "restored.*terminals" .daemon.log | tail -1 | grep -oE '[0-9]+'
```

**Error/Warning Count**:
```bash
tail -50 .daemon.log | grep -c "ERROR"
tail -50 .daemon.log | grep -c "WARN"
```

## Comparison: Old vs New Workflow

### Old Way (Manual)
```bash
# Terminal 1
cd loom-daemon
RUST_LOG=info cargo run
# Output scrolls by, hard to spot issues
# Manual restart needed for changes
# No metrics, just raw logs

# Terminal 2
pnpm tauri:dev
```

### New Way (Interactive)
```bash
# Terminal 1
pnpm run daemon:dev
# ✓ Color-coded logs
# ✓ Real-time metrics
# ✓ Auto-restart on Ctrl+C
# ✓ Connection monitoring
# ✓ Error highlighting

# Terminal 2
pnpm run tauri:dev
```

## Future Enhancements

Potential improvements to the monitoring interface:

1. **Interactive Commands**: Press keys to trigger actions
   - `r` - Restart daemon
   - `c` - Clear screen
   - `f` - Toggle log filtering
   - `s` - Show statistics

2. **Advanced Metrics**:
   - Request rate (requests/sec)
   - Memory usage (RSS)
   - CPU percentage
   - Throughput (bytes/sec)

3. **Terminal List View**: Press `t` to show terminal table
   ```
   ID       Name       Status    Uptime    Last Activity
   abc123   Shell      idle      00:05:23  2s ago
   def456   Worker 1   busy      00:03:15  0s ago
   ```

4. **Log Level Filtering**: Press `1-5` to change filter level
   - `1` - ERROR only
   - `2` - ERROR + WARN
   - `3` - ERROR + WARN + INFO
   - `4` - All (including DEBUG)
   - `5` - All + polling messages

5. **Graph View**: ASCII charts of metrics over time
   ```
   Connections [last 60s]:
   ┌────────────────────┐
   │     ╭─╮            │
   │  ╭──╯ ╰─╮          │
   │──╯      ╰──────────│
   └────────────────────┘
   ```

6. **Health Checks**: Automatic daemon health monitoring
   - Ping daemon every 5 seconds
   - Show warning if no response
   - Auto-restart option if daemon hangs

## Troubleshooting

### Daemon won't start
**Symptom**: "ERROR: Daemon failed to start"

**Solutions**:
- Check `.daemon.log` for errors
- Verify tmux is installed: `which tmux`
- Check port/socket not in use: `lsof ~/.loom/daemon.sock`
- Try manual start: `pnpm run daemon:run`

### Colors not showing
**Symptom**: See `[0;32m` instead of colors

**Solutions**:
- Ensure terminal supports ANSI colors
- Try different terminal app (iTerm2, Terminal.app)
- Check `TERM` environment variable: `echo $TERM`

### High CPU usage
**Symptom**: Script uses lots of CPU

**Cause**: Tail loop polling too fast or too many log lines

**Solutions**:
- Reduce log verbosity: Remove `RUST_LOG=info` from start script
- Filter out polling messages (already implemented in gray)
- Kill and restart with `Ctrl+C`

### Metrics not updating
**Symptom**: Status bar stuck or not refreshing

**Cause**: Log file not being written or tail command failed

**Solutions**:
- Verify `.daemon.log` exists and is growing: `ls -lh .daemon.log`
- Check tail process running: `ps aux | grep tail`
- Restart dev mode: `Ctrl+C` and `pnpm run daemon:dev`

## See Also

- [DEV_WORKFLOW.md](dev-workflow.md) - Complete development workflow guide
- [scripts/README.md](../../scripts/README.md) - Script reference documentation
- [DEVELOPMENT.md](development.md) - Code quality and testing
