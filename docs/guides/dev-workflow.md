# Development Workflow

This document describes the recommended workflow for working on `loom-daemon` and related tooling.

## Related Documentation

- [README.md](../../README.md) - Project overview and quick start
- [DEVELOPMENT.md](development.md) - Code quality and testing
- [daemon-dev-mode.md](daemon-dev-mode.md) - Detailed daemon dev mode reference
- [scripts/README.md](../../scripts/README.md) - Daemon script reference
- [CLAUDE.md](../../CLAUDE.md) - AI development context

## Quick Start

### Interactive Daemon Monitor

```bash
pnpm run daemon:dev
```

This gives you a **live monitoring dashboard** showing:
- Daemon status (running/stopped)
- Uptime counter
- Active connections
- Terminal count
- Errors and warnings
- Color-coded logs (errors=red, warnings=yellow, info=green)
- Real-time activity stream

Ctrl+C cleanly stops the daemon.

## Workflow

### 1. Make Code Changes

Edit Rust code in `loom-daemon/`, `loom-api/`, or TypeScript in `mcp-loom/`. Edit Python tools in `loom-tools/`. Edit shell scripts in `defaults/scripts/` or `scripts/`.

### 2. Rebuild and Test

For daemon changes:
```bash
pnpm run daemon:build          # release build
cargo test --workspace          # run all Rust tests
```

For mcp-loom changes:
```bash
cd mcp-loom && npm run build
```

For Python tool changes:
```bash
cd loom-tools && uv run pytest tests/ -x -q
```

### 3. Test the Daemon Locally

Restart the daemon to load the new binary:
```bash
pnpm run daemon:stop
pnpm run daemon:dev
```

Or run in the foreground without the monitor:
```bash
pnpm run daemon:preview
```

## Debugging Tips

### Check Daemon Health

```bash
# See if daemon process is running
ps aux | grep loom-daemon

# Check socket exists
ls -la ~/.loom/daemon.sock

# Tail daemon log
tail -f ~/.loom/daemon.log
```

### Check tmux Sessions

```bash
# List all tmux sessions (these are your terminals)
tmux ls

# Attach to a session manually to see its content
tmux attach -t loom-<id>

# Kill all sessions (nuclear option)
tmux kill-server
```

### When to Restart the Daemon

Restart the daemon if:
- You rebuilt `loom-daemon`
- Memory usage looks high
- State is corrupted
- You want a completely fresh start

## Available Commands

### Development

- `pnpm run daemon:dev` - **Interactive daemon monitor**
- `pnpm run daemon:headless` - Start daemon in background (silent)
- `pnpm run daemon:stop` - Stop daemon
- `pnpm run daemon:preview` - Run daemon in foreground (cargo run)

### Building

- `pnpm run daemon:build` - Build release daemon binary
- `cargo build --workspace` - Build everything in the Rust workspace

### Testing

- `pnpm run daemon:test` - Run daemon tests only
- `pnpm run test` - Run the full workspace test suite
- `pnpm run test:python` - Run Python tool tests

### Code Quality

- `pnpm run check:all` - Run all checks (format, clippy, build, test)
- `pnpm run format:rust:write` - Auto-format Rust code
- `pnpm run clippy` - Run Rust linter
- `pnpm run clippy:fix` - Auto-fix Rust linting issues
