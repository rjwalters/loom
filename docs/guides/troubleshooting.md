# Troubleshooting Guide

This guide helps you diagnose and fix common issues in Loom.

## Common Issues

### Installation Issues

**Symptom:** `./install.sh` fails during the Full Install workflow at Step 3: Creating Installation Worktree

**Error message:**
```
✗ Error: Invalid worktree path returned: HEAD is now at...
.loom/worktrees/issue-XXX
```

**Cause:** This was a bug in versions prior to v0.1.1 where the `create-worktree.sh` script didn't properly redirect git output, causing the installation script to receive multi-line output instead of the expected pipe-delimited format.

**Solution:**
1. **Update to latest version** (v0.1.1+):
   ```bash
   cd /path/to/loom
   git pull
   pnpm daemon:build
   ```

2. **Or manually fix** (if using older version):
   - Edit `scripts/install/create-worktree.sh`
   - Find line with `git worktree add -b "$BRANCH_NAME"`
   - Change `2>/dev/null` to `>&2 2>&1`
   - This redirects git's output to stderr, keeping stdout clean

3. **Cleanup and retry:**
   ```bash
   # Clean up any partial installation
   cd /path/to/target-repo
   git worktree list  # Find orphaned worktrees
   git worktree remove .loom/worktrees/issue-XXX --force
   git branch -D feature/loom-installation-X
   gh issue close XXX --comment "Retrying installation"

   # Retry installation
   /path/to/loom/install.sh /path/to/target-repo
   ```

**Note:** This issue only affects the Full Install workflow (Option 2). The Quick Install workflow (Option 1) is unaffected.

### Daemon Won't Start

**Symptom:** Red daemon indicator, "Disconnected" status in the UI

**Solutions:**

1. **Check daemon logs:**
   ```bash
   tail -f ~/.loom/daemon.log
   ```
   Look for error messages indicating why the daemon failed to start.

2. **Kill stale processes:**
   ```bash
   # Kill any running Loom processes
   pkill -f "loom-daemon"
   pkill -f "Loom"
   ```

3. **Remove stale socket:**
   ```bash
   rm -f ~/.loom/loom-daemon.sock
   ```

4. **Rebuild and restart:**
   ```bash
   pnpm daemon:build
   pnpm app:preview
   ```

### Terminals Not Responding

**Symptom:** Terminal shows "Busy" but nothing is happening

**Solutions:**

1. **Check terminal logs:**
   ```bash
   tail -f /tmp/loom-terminal-*.out
   ```

2. **List tmux sessions:**
   ```bash
   tmux -L loom list-sessions
   ```

3. **Attach manually to debug:**
   ```bash
   tmux -L loom attach -t loom-terminal-1
   ```
   Press `Ctrl+b d` to detach without killing the session.

4. **Check health status:**
   Use MCP: `mcp__loom__read_state_file` to see terminal health status.

### Agent Won't Launch

**Symptom:** Agent terminal stuck at launch prompt or crashes immediately

**Solutions:**

1. **Check console logs:**
   ```bash
   mcp__loom__read_console_log
   ```
   Look for agent launch errors.

2. **Verify role file exists:**
   ```bash
   ls -la defaults/roles/
   ls -la .loom/roles/  # Custom roles
   ```

3. **Check GitHub CLI authentication:**
   ```bash
   gh auth status
   ```
   If not authenticated:
   ```bash
   gh auth login
   ```

4. **Verify Claude Code installation:**
   ```bash
   which claude
   ```
   If not found, install Claude Code.

5. **Check working directory:**
   Ensure you're in a valid git repository:
   ```bash
   git status
   ```

### Worktree Creation Fails

**Symptom:** `pnpm worktree <issue>` fails with errors

**Common errors and solutions:**

1. **"fatal: 'path' already exists"**
   ```bash
   # Check if worktree exists
   git worktree list

   # Remove if orphaned
   git worktree remove .loom/worktrees/issue-<number> --force

   # Or if that fails
   rm -rf .loom/worktrees/issue-<number>
   git worktree prune
   ```

2. **"already used by worktree"**
   ```bash
   # List all worktrees
   git worktree list

   # Navigate to main workspace first
   cd /path/to/loom
   pnpm worktree <issue>
   ```

3. **"branch already exists"**
   ```bash
   # Delete existing branch
   git branch -D feature/issue-<number>

   # Try again
   pnpm worktree <issue>
   ```

### Factory Reset Hangs or Fails

**Symptom:** Factory reset gets stuck or terminals don't launch

**Solutions:**

1. **Check console logs for stuck operations:**
   ```bash
   mcp__loom__read_console_log
   ```

2. **Manually kill tmux sessions:**
   ```bash
   tmux -L loom kill-server
   ```

3. **Reset state files:**
   ```bash
   rm -f .loom/state.json
   rm -f .loom/config.json
   ```

4. **Force restart:**
   Close the app completely and restart.

### CI Failures on PR

**Symptom:** GitHub Actions failing on your pull request

**Solutions:**

1. **Run CI locally first:**
   ```bash
   pnpm check:ci
   ```
   This runs the exact same checks as GitHub Actions.

2. **Common failures:**
   - **Biome lint/format:** Run `pnpm lint --write` and `pnpm format --write`
   - **Clippy warnings:** Run `pnpm clippy:fix` for auto-fixable issues
   - **Rust format:** Run `pnpm format:rust:write`
   - **TypeScript errors:** Run `pnpm exec tsc --noEmit` to check types
   - **Build failures:** Ensure daemon is built: `pnpm daemon:build`

3. **Check specific job logs:**
   ```bash
   gh pr checks <pr-number>
   gh run view <run-id> --log-failed
   ```

## Debugging Tools

### MCP Servers

Loom provides a unified MCP server (`mcp-loom`) for AI-powered debugging:

#### UI Tools
- `mcp__loom__read_console_log` - Read frontend console logs
- `mcp__loom__read_state_file` - Read application state
- `mcp__loom__read_config_file` - Read terminal configurations
- `mcp__loom__trigger_force_start` - Start engine without confirmation
- `mcp__loom__trigger_factory_reset` - Reset to factory defaults
- `mcp__loom__get_heartbeat` - Check app health status

#### Log Tools
- `mcp__loom__tail_daemon_log` - Read daemon logs
- `mcp__loom__tail_tauri_log` - Read Tauri app logs
- `mcp__loom__list_terminal_logs` - List terminal log files
- `mcp__loom__tail_terminal_log` - Read specific terminal logs

#### Terminal Tools
- `mcp__loom__list_terminals` - List all terminals
- `mcp__loom__get_selected_terminal` - Get primary terminal info
- `mcp__loom__get_terminal_output` - Read terminal output
- `mcp__loom__send_terminal_input` - Send input to terminal

See [MCP Documentation](../mcp/README.md) for full API reference.

### Log Locations

- **Frontend console:** `~/.loom/console.log` (JSON structured logs)
- **Daemon:** `~/.loom/daemon.log` (JSON structured logs)
- **Tauri app:** `~/.loom/tauri.log` (Tauri application logs)
- **Terminal output:** `/tmp/loom-terminal-*.out` (raw terminal output)

### Log Querying with jq

```bash
# Filter by component
jq 'select(.context.component == "worktree-manager")' ~/.loom/console.log

# Find errors
jq 'select(.level == "ERROR")' ~/.loom/console.log

# Track specific terminal
jq 'select(.context.terminalId == "terminal-1")' ~/.loom/console.log

# Find by error ID
jq 'select(.context.errorId == "ERR-abc123")' ~/.loom/console.log

# Last 10 errors with context
jq 'select(.level == "ERROR")' ~/.loom/console.log | tail -10
```

### Inspecting tmux Sessions

```bash
# List all Loom tmux sessions
tmux -L loom list-sessions

# Attach to a specific session
tmux -L loom attach -t loom-terminal-1

# Kill a specific session
tmux -L loom kill-session -t loom-terminal-1

# Kill all Loom sessions
tmux -L loom kill-server
```

### Git Worktree Inspection

```bash
# List all worktrees
git worktree list

# Remove orphaned worktrees
git worktree prune

# Check worktree status
cd .loom/worktrees/issue-<number>
git status
git log --oneline -5
```

## Performance Issues

### Slow UI Updates

**Symptom:** UI feels laggy or unresponsive

**Solutions:**

1. **Check number of terminals:**
   - Limit to 10-15 terminals for best performance
   - Close unused terminals

2. **Check daemon load:**
   ```bash
   ps aux | grep loom-daemon
   ```

3. **Restart the app:**
   Sometimes helps clear accumulated state.

### High CPU Usage

**Symptom:** Loom using excessive CPU

**Solutions:**

1. **Check for stuck agents:**
   Use MCP to check terminal status and kill stuck processes.

2. **Check tmux sessions:**
   ```bash
   tmux -L loom list-sessions
   ```
   Kill sessions that shouldn't be running.

3. **Check daemon logs:**
   Look for infinite loops or errors:
   ```bash
   tail -f ~/.loom/daemon.log
   ```

## Getting Help

### Before Opening an Issue

1. **Search existing issues:**
   ```bash
   gh issue list --search "your error message"
   ```

2. **Check documentation:**
   - [Guides](../guides/)
   - [ADRs](../adr/)
   - [MCP API](../mcp/)

3. **Gather diagnostic info:**
   - System info: `sw_vers`, `uname -a`
   - tmux version: `tmux -V`
   - Node version: `node -v`
   - Relevant log output from `~/.loom/*.log`

### Opening an Issue

When opening a bug report, include:

1. **Steps to reproduce:**
   - What you did
   - What you expected
   - What actually happened

2. **Environment:**
   - macOS version
   - Loom version (git commit hash)
   - Node/pnpm versions

3. **Logs:**
   - Relevant excerpts from log files
   - Use code blocks for formatting
   - Include error IDs if present in logs

4. **Screenshots (if applicable):**
   - UI state
   - Error messages

### Support Channels

- [GitHub Issues](https://github.com/rjwalters/loom/issues) - Bug reports and feature requests
- [GitHub Discussions](https://github.com/rjwalters/loom/discussions) - Questions and community support
- [Documentation](https://github.com/rjwalters/loom/tree/main/docs) - Guides and references

## Advanced Debugging

### Using the Rust Debugger

For debugging the daemon or Tauri backend:

```bash
# Build with debug symbols
cargo build --package loom-daemon

# Run with debugger
lldb target/debug/loom-daemon
```

### Frontend DevTools

Open the Tauri DevTools:
- Press `Cmd+Option+I` in the running app
- Inspect console, network, and application state

### Trace Mode

Enable verbose logging:

```bash
# Set environment variable before starting
export RUST_LOG=debug
pnpm app:preview
```

Check `~/.loom/daemon.log` for detailed trace output.
