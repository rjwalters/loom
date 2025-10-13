# Loom Development Scripts

This directory contains shell scripts for managing the daemon during development.

## Scripts

### dev-daemon.sh
**Interactive development mode** - Starts daemon and provides live monitoring dashboard.

- **PID file**: `.daemon.pid` (in project root)
- **Log file**: `.daemon.log` (in project root)
- **Interactive**: Keeps terminal active with colored log streaming
- **Metrics**: Shows uptime, connections, terminals, errors, warnings
- **Auto-cleanup**: Stops daemon on Ctrl+C

Features:
- Color-coded log output (errors=red, warnings=yellow, info=green)
- Real-time activity monitoring
- Connection tracking
- Error/warning counters
- Uptime display

Usage:
```bash
./scripts/dev-daemon.sh
# or
pnpm run daemon:dev
```

**Recommended for development**: Use this in one terminal while running `pnpm tauri:dev` in another.

### start-daemon.sh
Starts the daemon in the background silently and stores its PID.

- **PID file**: `.daemon.pid` (in project root)
- **Log file**: `.daemon.log` (in project root)
- **Idempotent**: Won't start if already running
- **Verification**: Checks that process started successfully

Usage:
```bash
./scripts/start-daemon.sh
# or
pnpm run daemon:start
```

### stop-daemon.sh
Stops the daemon gracefully (or force kills if needed).

- Reads PID from `.daemon.pid`
- Sends SIGTERM first (graceful)
- Waits up to 5 seconds for process to die
- Force kills with SIGKILL if still running
- Cleans up PID file
- Fallback: Searches for process by name if PID file missing

Usage:
```bash
./scripts/stop-daemon.sh
# or
pnpm run daemon:stop
```

### restart-daemon.sh
Restarts the daemon (stop + wait + start).

Equivalent to:
```bash
./scripts/stop-daemon.sh
sleep 1
./scripts/start-daemon.sh
```

Usage:
```bash
./scripts/restart-daemon.sh
# or
pnpm run daemon:restart
```

## Integration with pnpm

These scripts are used by the pnpm commands in `package.json`:

| Command | Description | Use Case |
|---------|-------------|----------|
| `pnpm run daemon:dev` | **Interactive dev mode** (recommended) | Two-terminal development workflow |
| `pnpm run app:dev` | Start daemon + Tauri dev (all-in-one) | One-command automated startup |
| `pnpm run app:dev:restart` | Restart daemon only | When daemon gets into bad state |
| `pnpm run app:stop` | Stop daemon | Clean shutdown |
| `pnpm run daemon:start` | Start daemon in background (silent) | Scripting/automation |
| `pnpm run daemon:stop` | Stop daemon | Manual control |
| `pnpm run daemon:restart` | Restart daemon | Manual recovery |
| `pnpm run daemon:run` | Run daemon in foreground (cargo run) | Low-level debugging |

## Files Created

These scripts create files in the project root (all gitignored):

- `.daemon.pid` - Process ID of running daemon
- `.daemon.log` - Daemon stdout/stderr output

## Troubleshooting

### Daemon won't start
Check the log file:
```bash
cat .daemon.log
```

Common issues:
- Port/socket already in use
- Cargo build failed
- Permissions issue

### Daemon won't stop
Force kill manually:
```bash
# Find PID
ps aux | grep loom-daemon

# Kill it
kill -9 <PID>

# Clean up
rm -f .daemon.pid
```

### Stale PID file
If `.daemon.pid` exists but daemon isn't running:
```bash
rm .daemon.pid
pnpm run daemon:start
```

The stop script handles this automatically by checking if the PID is still alive.

## Development Notes

- Scripts use `set -e` to exit on error
- PID is verified after starting (1 second grace period)
- Graceful shutdown with 5 second timeout
- Process detection by name as fallback
- RUST_LOG=info for informative logging
