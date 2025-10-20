# Factory Reset Testing Loop

## Overview

This document describes the comprehensive testing procedure for validating Loom's factory reset and workspace startup functionality. The goal is to achieve **100% reliability** in creating and managing terminal sessions with AI agents.

## Test Objectives

1. **Clean State Initialization**: Ensure no stale processes or resources before testing
2. **Successful Startup**: All terminals launch with Claude Code agents running
3. **Factory Reset Reliability**: Terminals cleanly restart after factory reset
4. **Session Health**: All terminals remain responsive and properly tracked

## Prerequisites

### Required Tools

- **MCP Servers**: Three MCP servers provide comprehensive observability
  - `mcp-loom-ui`: UI interaction and state inspection
  - `mcp-loom-logs`: Log file monitoring (daemon, Tauri, terminal output)
  - `mcp-loom-terminals`: Direct terminal IPC control

- **Logging Infrastructure**: Structured JSON logs for debugging
  - Frontend console: `~/.loom/console.log`
  - Daemon logs: `~/.loom/daemon.log`
  - Tauri logs: `~/.loom/tauri.log`
  - Terminal output: `/tmp/loom-terminal-{id}.out`

### Environment Setup

```bash
# Ensure you're in the Loom workspace
cd /Users/rwalters/GitHub/loom

# Check that all dependencies are installed
pnpm install
```

### MCP Server Connection

**IMPORTANT**: The testing procedures in this document rely on MCP (Model Context Protocol) servers for observability and control. You must verify MCP servers are connected before starting tests.

**Verifying MCP Connection**:

1. **Check MCP Configuration** exists in workspace:
   ```bash
   cat .mcp.json
   ```

   Expected output should show three MCP servers configured:
   - `loom-ui`: UI interaction and state inspection
   - `loom-logs`: Log file monitoring
   - `loom-terminals`: Terminal IPC control

2. **Test MCP Server Availability** (from Claude Code or MCP client):
   ```bash
   # Test if MCP commands are available
   mcp__loom-ui__get_heartbeat
   ```

   If this returns a heartbeat response, MCP servers are connected.

**If MCP Connection Fails**:

1. **Ensure MCP servers are built**:
   ```bash
   # Build all MCP servers
   cd mcp-loom-ui && pnpm install && pnpm build && cd ..
   cd mcp-loom-logs && pnpm install && pnpm build && cd ..
   cd mcp-loom-terminals && pnpm install && pnpm build && cd ..
   ```

2. **Restart Claude Code** to reload MCP configuration:
   - Exit Claude Code completely
   - Restart Claude Code in the Loom workspace

3. **Check MCP server logs** for errors:
   ```bash
   # MCP servers log to stderr, visible in Claude Code console
   # Look for connection errors or missing dependencies
   ```

**File-Based MCP Command Triggering** (Recommended Alternative):

Loom implements a file-based command system that works reliably regardless of MCP stdio connection status. This is the **recommended testing method** when MCP servers aren't connected to your Claude Code session.

**How it works:**
1. Write command to `~/.loom/mcp-command.json`
2. Loom's file watcher (using `notify` crate) detects the change
3. Loom executes the command and emits appropriate events
4. Loom writes acknowledgment to `~/.loom/mcp-ack.json`

**Usage:**

```bash
# Trigger force start (bypasses confirmation dialog)
echo '{"command": "trigger_force_start", "timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' > ~/.loom/mcp-command.json

# Wait for acknowledgment
sleep 2
cat ~/.loom/mcp-ack.json

# Trigger factory reset
echo '{"command": "trigger_factory_reset", "timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' > ~/.loom/mcp-command.json
```

**Available commands:**
- `trigger_start` - Start workspace (shows confirmation)
- `trigger_force_start` - Start workspace (no confirmation)
- `trigger_factory_reset` - Reset workspace (shows confirmation)
- `trigger_force_factory_reset` - Reset workspace (no confirmation)

**Troubleshooting MCP stdio connection:**

If `mcp__loom-ui__*` commands show "No such tool available", check:

1. **Verify Claude Code loaded MCP config**: MCP servers are only loaded when Claude Code starts
   ```bash
   # Check if MCP servers are attached to current session
   ps -p $PPID -o pid,ppid,comm
   ps -ef | grep "mcp-loom" | grep "$(ps -p $PPID -o ppid=)"
   ```

2. **Multiple Claude Code sessions**: If you see orphaned MCP server processes, they may be connected to old sessions:
   ```bash
   # Count running MCP server processes
   ps aux | grep "mcp-loom" | grep -v grep | wc -l
   # Should be 3 (one set), not 6, 9, 15, 21, etc.
   ```

3. **Solution**: Restart Claude Code to reload MCP configuration, OR use file-based commands (which always work)

**Manual Monitoring Methods** (if file-based commands unavailable):

1. **Console Logs** - Monitor `~/.loom/console.log` directly:
   ```bash
   tail -f ~/.loom/console.log
   ```

2. **Daemon Logs** - Monitor daemon activity:
   ```bash
   tail -f ~/.loom/daemon.log
   ```

3. **tmux Sessions** - Check terminal sessions directly:
   ```bash
   tmux -L loom list-sessions
   tmux -L loom attach -t loom-terminal-1
   ```

4. **Terminal Output Files** - Check individual terminal logs:
   ```bash
   tail -f /tmp/loom-terminal-1.out
   ```

**Note**: File-based command triggering is the most reliable method. MCP stdio connection is a convenience wrapper that may not always be available depending on session state.

## Testing Loop Phases

### Phase 1: Clean Slate Reset

**Goal**: Eliminate all stale state and processes

**Steps**:

1. **Kill Running Processes**
   ```bash
   # Kill any running Loom app instances
   pkill -f "loom" || true

   # Kill any running daemon instances
   pkill -f "loom-daemon" || true

   # Wait for processes to fully terminate
   sleep 2
   ```

2. **Clean Socket Files**
   ```bash
   # Remove stale Unix domain sockets
   rm -f /tmp/loom-daemon.sock
   rm -f /tmp/loom-daemon-*.sock

   # Verify sockets are gone
   ls -la /tmp/loom-daemon* 2>/dev/null || echo "✓ No stale sockets"
   ```

3. **Clean tmux Sessions**
   ```bash
   # List all loom tmux sessions
   tmux -L loom list-sessions 2>/dev/null || echo "✓ No tmux sessions"

   # Kill all loom tmux sessions if any exist
   tmux -L loom kill-server 2>/dev/null || true

   # Verify all sessions are gone
   tmux -L loom list-sessions 2>/dev/null && echo "⚠ WARNING: tmux sessions still exist" || echo "✓ All tmux sessions cleaned"
   ```

4. **Verify Clean State**
   ```bash
   # Check for any remaining processes
   ps aux | grep -i loom | grep -v grep || echo "✓ No Loom processes running"

   # Check for any remaining sockets
   ls -la /tmp/loom* 2>/dev/null || echo "✓ No Loom sockets"

   # Check for any remaining tmux sessions
   tmux -L loom list-sessions 2>/dev/null || echo "✓ No tmux sessions"
   ```

**Success Criteria**:
- ✅ No Loom processes running
- ✅ No socket files in `/tmp/`
- ✅ No tmux sessions (loom server)
- ✅ Clean terminal output logs

### Phase 2: Build and Launch

**Goal**: Start Loom with a fresh build attached to the current workspace

**Steps**:

1. **Build and Launch with Workspace Argument** (Recommended)
   ```bash
   # Run full build and launch with workspace attachment
   pnpm test:factory-reset

   # Alternative: Use app:preview (same as test:factory-reset)
   pnpm app:preview
   ```

   Both scripts:
   - Build the frontend (TypeScript + Vite)
   - Build the Tauri debug bundle
   - Start daemon with 5-second initialization wait
   - Launch app attached to current directory as workspace

   **Note**: The `--workspace` argument is automatically added by these scripts, ensuring Loom attaches to the current directory. This is critical for reliable terminal setup.

2. **Alternative: Manual Launch with Workspace**
   ```bash
   # Build only (without launching)
   pnpm build && pnpm tauri build --debug --bundles app

   # Start daemon
   RUST_LOG=info pnpm daemon:preview &

   # Wait for daemon to be ready (5 seconds recommended)
   sleep 5

   # Launch with workspace argument
   ./target/debug/bundle/macos/Loom.app/Contents/MacOS/Loom --workspace "$(pwd)"
   ```

3. **Monitor Startup Logs** (via MCP)
   ```bash
   # Watch daemon logs for startup sequence
   mcp__loom-logs__tail_daemon_log --lines=50

   # Watch Tauri logs for app initialization
   mcp__loom-logs__tail_tauri_log --lines=50

   # Watch console logs for frontend initialization
   mcp__loom-ui__read_console_log
   ```

3. **Wait for App Launch**
   - **Expected**: App window appears within 10 seconds
   - **Expected**: Daemon starts automatically with 5-second initialization delay
   - **Expected**: Socket file created at `/tmp/loom-daemon.sock`
   - **Expected**: Workspace automatically selected (displays current directory path)

**Success Criteria**:
- ✅ App window visible
- ✅ Workspace attached to current directory
- ✅ Daemon process running (check with `ps aux | grep loom-daemon`)
- ✅ Socket file exists: `/tmp/loom-daemon.sock`
- ✅ No "workspace not selected" errors in UI
- ✅ No error messages in logs

### Phase 3: Workspace Startup & Agent Launch

**Goal**: Trigger workspace start and verify all terminals launch successfully

**Steps**:

1. **Trigger Workspace Start**

   **Method A: File-Based Command** (Recommended - always works):
   ```bash
   # Trigger workspace start via file-based command
   echo '{"command": "trigger_force_start", "timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' > ~/.loom/mcp-command.json

   # Wait for acknowledgment
   sleep 2
   cat ~/.loom/mcp-ack.json
   ```

   **Method B: MCP Tool** (if MCP servers connected):
   ```bash
   # Use force_start to bypass confirmation dialog
   mcp__loom-ui__trigger_force_start
   ```

2. **Monitor Console Logs** (real-time)

   **Method A: Direct File Read** (Recommended):
   ```bash
   # Watch for terminal creation sequence
   tail -50 ~/.loom/console.log | grep -E "start-workspace|Created terminal|Claude Code"
   ```

   **Method B: MCP Tool** (if available):
   ```bash
   mcp__loom-ui__read_console_log
   ```

   **Expected log sequence**:
   ```
   [start-workspace] Killing all loom tmux sessions
   [start-workspace] ✓ Created terminal-1
   [start-workspace] ✓ Created terminal-2
   ...
   [start-workspace] ✓ Created terminal-9
   [launchAgentInTerminal] ✓ Agent will start in main workspace
   [launchAgentInTerminal] Sending "2" to accept warning
   [launchAgentInTerminal] ✓ Claude Code launched successfully
   ```

3. **Verify Terminal State**

   **Method A: Direct File Read** (Recommended):
   ```bash
   # Check application state
   cat .loom/state.json | head -50
   ```

   **Method B: MCP Tool** (if available):
   ```bash
   mcp__loom-ui__read_state_file
   ```

   **Expected state**:
   ```json
   {
     "terminals": [
       {
         "id": "terminal-1",
         "name": "Shell",
         "status": "idle",
         "sessionId": "loom-terminal-1",
         "workingDirectory": "/Users/rwalters/GitHub/loom"
       },
       // ... 8 more terminals (terminal-2 through terminal-9)
     ]
   }
   ```

4. **Verify tmux Sessions**

   **Method A: Direct tmux Check** (Recommended):
   ```bash
   # List all terminal sessions
   tmux -L loom list-sessions

   # Count sessions (should be 9)
   tmux -L loom list-sessions 2>/dev/null | wc -l
   ```

   **Method B: MCP Tool** (if available):
   ```bash
   mcp__loom-terminals__list_terminals
   ```

   **Expected output**:
   ```json
   {
     "terminals": [
       {
         "id": "terminal-1",
         "sessionId": "loom-terminal-1",
         "workingDirectory": "/Users/rwalters/GitHub/loom"
       },
       // ... 6 more terminals
     ]
   }
   ```

5. **Check Terminal Output** (for each terminal)
   ```bash
   # Check terminal-1 output
   mcp__loom-logs__tail_terminal_log --terminal-id=terminal-1 --lines=50

   # Repeat for terminal-2 through terminal-7
   ```

   **Expected in each terminal**:
   - Claude Code startup banner
   - "WARNING: Claude Code running in Bypass Permissions mode"
   - "Enter 1 to view and approve, 2 to approve, 0 to cancel"
   - Acceptance of warning (automatic via agent launcher)
   - Claude Code interactive prompt

**Success Criteria**:
- ✅ 9 terminals created (terminal-1 through terminal-9)
- ✅ All terminals in `idle` or `busy` status (not `error`)
- ✅ All terminals have valid `sessionId` (no `null` or missing)
- ✅ All terminals start in main workspace directory
- ✅ Claude Code running in all 9 terminals
- ✅ No "command not found" errors
- ✅ No "duplicate session" errors
- ✅ No stale error overlays visible in UI

### Phase 4: Factory Reset Test

**Goal**: Trigger factory reset and verify clean recovery

**Steps**:

1. **Trigger Factory Reset**

   **Method A: File-Based Command** (Recommended):
   ```bash
   # Reset workspace to defaults (does NOT auto-start)
   echo '{"command": "trigger_factory_reset", "timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' > ~/.loom/mcp-command.json

   # Wait for acknowledgment
   sleep 2
   cat ~/.loom/mcp-ack.json

   # Watch console logs for reset progress
   tail -50 ~/.loom/console.log | grep -E "workspace-reset|Destroying|Configuration"
   ```

   **Method B: MCP Tool** (if available):
   ```bash
   mcp__loom-ui__trigger_factory_reset
   ```

   **Expected log sequence**:
   ```
   [workspace-reset] Starting workspace reset
   [workspace-reset] Killing all loom tmux sessions
   [workspace-reset] ✓ Killed all sessions
   [workspace-reset] Destroying terminal session for terminal-1
   ...
   [workspace-reset] Destroying terminal session for terminal-9
   [workspace-reset] ✓ All terminals destroyed
   [workspace-reset] Resetting configuration to defaults
   [workspace-reset] ✓ Configuration reset complete
   ```

2. **Verify Clean State After Reset**

   **Method A: Direct Verification** (Recommended):
   ```bash
   # Check that all terminals are destroyed
   cat .loom/state.json

   # Verify no tmux sessions remain
   tmux -L loom list-sessions 2>/dev/null || echo "✓ No sessions"

   # Check daemon logs for errors
   tail -50 ~/.loom/daemon.log | grep -i error
   ```

   **Method B: MCP Tools** (if available):
   ```bash
   mcp__loom-ui__read_state_file
   mcp__loom-terminals__list_terminals
   mcp__loom-logs__tail_daemon_log --lines=50
   ```

3. **Restart Workspace**

   **Method A: File-Based Command** (Recommended):
   ```bash
   # Start engine with reset configuration
   echo '{"command": "trigger_force_start", "timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' > ~/.loom/mcp-command.json

   # Wait for acknowledgment
   sleep 2
   cat ~/.loom/mcp-ack.json
   ```

   **Method B: MCP Tool** (if available):
   ```bash
   mcp__loom-ui__trigger_force_start
   ```

4. **Repeat Phase 3 Verification**
   - Follow all steps from Phase 3 to verify terminals launch successfully
   - Confirm no stale state from previous session

**Success Criteria**:
- ✅ All terminals cleanly destroyed during reset
- ✅ No orphaned tmux sessions after reset
- ✅ Configuration properly reset to defaults
- ✅ Workspace restart creates 7 fresh terminals
- ✅ All agents launch successfully in new terminals
- ✅ No errors or warnings in logs

## Common Issues & Debugging

### Issue: App launches without workspace attached

**Symptom**: Loom window shows "Select a workspace" despite being in a git repository

**Root Cause**: App launched without `--workspace` argument

**Debug Steps**:
```bash
# Check if app was launched with workspace argument
ps aux | grep Loom | grep workspace || echo "⚠ WARNING: No workspace argument"

# Check console logs for workspace selection
mcp__loom-ui__read_console_log | grep workspace
```

**Fix**: Always use the provided pnpm scripts that include workspace argument:
```bash
# Use these scripts (they include --workspace argument)
pnpm test:factory-reset
pnpm app:preview

# DON'T launch directly without workspace:
# ❌ ./target/debug/bundle/macos/Loom.app/Contents/MacOS/Loom
```

**Impact**: Without workspace attachment, terminals will fail to create or will be created in the wrong directory, causing unreliable agent setup.

### Issue: "duplicate session" errors

**Symptom**: `fatal: duplicate session: loom-terminal-X`

**Root Cause**: tmux sessions not properly cleaned up before creating new ones

**Debug Steps**:
```bash
# Check for existing sessions
mcp__loom-terminals__list_terminals

# Check daemon logs for kill_all_loom_sessions calls
mcp__loom-logs__tail_daemon_log --lines=100 | grep "kill.*session"

# Manual cleanup if needed
tmux -L loom kill-server
```

**Fix**: Ensure `kill_all_loom_sessions` runs before terminal creation

### Issue: Claude Code not launching

**Symptom**: Terminals stuck at shell prompt, no Claude Code banner

**Root Cause**: `claude` command not found or failed to launch

**Debug Steps**:
```bash
# Check terminal output for errors
mcp__loom-logs__tail_terminal_log --terminal-id=terminal-1 --lines=100

# Check console logs for agent launcher errors
mcp__loom-ui__read_console_log | grep "launchAgent"

# Verify Claude Code is installed
which claude
```

**Fix**:
- Ensure Claude Code CLI is installed and in PATH
- Check agent launcher retry logic in `src/lib/agent-launcher.ts`

### Issue: Bypass permissions prompt not accepted

**Symptom**: Terminals stuck at "WARNING: Claude Code running in Bypass Permissions mode"

**Root Cause**: Automatic acceptance not working, timing issues

**Debug Steps**:
```bash
# Check terminal output for prompt timing
mcp__loom-logs__tail_terminal_log --terminal-id=terminal-1 --lines=100

# Check console logs for send_terminal_input calls
mcp__loom-ui__read_console_log | grep "send_terminal_input"

# Check daemon logs for IPC operations
mcp__loom-logs__tail_daemon_log --lines=100 | grep "SendTerminalInput"
```

**Fix**: Adjust retry delays in `src/lib/agent-launcher.ts`

### Issue: Missing session overlay persists

**Symptom**: Error overlay visible behind xterm terminal

**Root Cause**: Error overlay HTML not cleared when session recovered

**Debug Steps**:
```bash
# Check if clearMissingSessionError is being called
mcp__loom-ui__read_console_log | grep "clearMissingSession"

# Check terminal state for missingSession flag
mcp__loom-ui__read_state_file | grep "missingSession"
```

**Fix**: Verify `clearMissingSessionError()` is called in `renderPrimaryTerminal()` when `hasMissingSession` is false

### Issue: Stale sockets preventing startup

**Symptom**: Daemon fails to start, "Address already in use" errors

**Root Cause**: Previous daemon didn't clean up socket file

**Debug Steps**:
```bash
# Check for stale sockets
ls -la /tmp/loom-daemon*

# Check if any process is using the socket
lsof /tmp/loom-daemon.sock
```

**Fix**:
```bash
# Remove stale socket
rm -f /tmp/loom-daemon.sock

# Restart app
```

### Issue: App launch fails with "Connection refused"

**Symptom**: App launches before daemon socket is ready, connection fails

**Root Cause**: Insufficient wait time for daemon initialization

**Fix**: The `pnpm app:preview` and `pnpm test:factory-reset` scripts now use a 5-second wait (increased from 2 seconds) to ensure daemon socket is ready before app launches.

If you still experience connection issues:
```bash
# Manually verify daemon is running
ps aux | grep loom-daemon

# Check if socket exists
ls -la /tmp/loom-daemon.sock

# If socket doesn't exist, wait a bit longer before launching
sleep 3
./target/debug/bundle/macos/Loom.app/Contents/MacOS/Loom --workspace $(pwd)
```

## Success Metrics

### Target: 100% Success Rate

**Current Status**: Track success rate over multiple test runs

| Test Run | Phase 1 | Phase 2 | Phase 3 | Phase 4 | Overall |
|----------|---------|---------|---------|---------|---------|
| Run 1    | ✅      | ✅      | ✅      | ✅      | ✅      |
| Run 2    | ✅      | ✅      | ✅      | ✅      | ✅      |
| Run 3    | ✅      | ✅      | ✅      | ✅      | ✅      |
| ...      | ...     | ...     | ...     | ...     | ...     |

**Goal**: 10 consecutive successful runs with no errors

### Key Performance Indicators

1. **Startup Time**: Time from app launch to all agents ready
   - **Target**: < 30 seconds
   - **Measure**: First log entry to last "Claude Code launched successfully"

2. **Factory Reset Time**: Time from reset trigger to all agents ready
   - **Target**: < 20 seconds
   - **Measure**: Reset trigger to last agent launch

3. **Agent Success Rate**: Percentage of terminals with successful agent launch
   - **Target**: 100% (9/9 terminals)
   - **Measure**: Count of "Claude Code launched successfully" logs

4. **Error Rate**: Number of errors per test run
   - **Target**: 0 errors
   - **Measure**: Count of ERROR-level log entries

## Automated Testing Script

```bash
#!/bin/bash
# factory-reset-test.sh - Automated testing loop

echo "=== Loom Factory Reset Testing Loop ==="
echo ""

# Phase 1: Clean Slate
echo "Phase 1: Clean Slate Reset"
pkill -f "loom" 2>/dev/null || true
pkill -f "loom-daemon" 2>/dev/null || true
sleep 2
rm -f /tmp/loom-daemon*.sock
tmux -L loom kill-server 2>/dev/null || true
echo "✓ Clean slate achieved"
echo ""

# Phase 2: Build and Launch
echo "Phase 2: Build and Launch"
echo "Starting app:preview..."
pnpm app:preview &
APP_PID=$!
echo "✓ App launching (PID: $APP_PID)"
echo ""

# Phase 3: Workspace Startup (via MCP)
echo "Phase 3: Workspace Startup"
echo "Waiting 10 seconds for app to initialize..."
sleep 10
echo "Triggering workspace start via MCP..."
# Note: MCP commands must be run from Claude Code or another MCP client
echo "✓ Ready for MCP-based testing"
echo ""

echo "=== Manual Steps Required ==="
echo "1. Use MCP to trigger: mcp__loom-ui__trigger_force_start"
echo "2. Monitor logs via: mcp__loom-ui__read_console_log"
echo "3. Verify state via: mcp__loom-ui__read_state_file"
echo "4. Trigger reset via: mcp__loom-ui__trigger_factory_reset"
echo "5. Repeat steps 1-3"
```

### Automated Integration Test (pnpm script)

You can execute the end-to-end factory reset regression test directly from any Bash terminal (including Loom MCP terminals) using the existing package script:

```bash
# Run the loom-daemon factory reset integration test
pnpm daemon:test -- --test integration_factory_reset -- --test-threads=1
```

This wraps `cargo test --test integration_factory_reset` so it works with `pnpm mcp__` invocations and local shells alike. The test mirrors the manual loop above: it launches the seven workspace terminals, verifies tmux sessions, destroys them, and repeats to ensure a clean slate.

## Continuous Improvement

### After Each Test Run

1. **Document Issues**: Record any failures or anomalies
2. **Update Metrics**: Track success rates and timing
3. **Refine Process**: Improve cleanup or startup procedures
4. **Fix Bugs**: Address root causes of failures

### Iteration Goal

- Run test loop repeatedly until 100% reliable
- Fix each bug as it appears
- Add regression tests for fixed issues
- Achieve 10 consecutive flawless runs

---

**Last Updated**: 2025-10-17 (Issue #294)
**Status**: Active Testing
**Next Milestone**: 10 consecutive successful runs

**Recent Improvements**:
- Added `pnpm test:factory-reset` script for workspace-attached testing
- Increased daemon startup wait time from 2s to 5s for better reliability
- Added comprehensive MCP server connection documentation
- Documented alternative testing methods when MCP unavailable
