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

# Verify MCP servers are configured
cat .mcp.json

# Check that all dependencies are installed
pnpm install
```

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

**Goal**: Start Loom with a fresh build in preview mode

**Steps**:

1. **Build the Application**
   ```bash
   # Run full build (frontend + Tauri binary)
   pnpm app:preview
   ```

2. **Monitor Startup Logs** (via MCP)
   ```bash
   # Watch daemon logs for startup sequence
   mcp__loom-logs__tail_daemon_log --lines=50

   # Watch Tauri logs for app initialization
   mcp__loom-logs__tail_tauri_log --lines=50

   # Watch console logs for frontend initialization
   mcp__loom-ui__read_console_log
   ```

3. **Wait for App Launch**
   - **Expected**: App window appears
   - **Expected**: Daemon starts automatically (spawned by Tauri)
   - **Expected**: Socket file created at `/tmp/loom-daemon.sock`

**Success Criteria**:
- ✅ App window visible
- ✅ Daemon process running (check with `ps aux | grep loom-daemon`)
- ✅ Socket file exists: `/tmp/loom-daemon.sock`
- ✅ No error messages in logs

### Phase 3: Workspace Startup & Agent Launch

**Goal**: Trigger workspace start and verify all terminals launch successfully

**Steps**:

1. **Trigger Workspace Start** (via MCP)
   ```bash
   # Use force_start to bypass confirmation dialog (for automation)
   mcp__loom-ui__trigger_force_start
   ```

2. **Monitor Console Logs** (real-time)
   ```bash
   # Watch for terminal creation sequence
   mcp__loom-ui__read_console_log
   ```

   **Expected log sequence**:
   ```
   [start-workspace] Killing all loom tmux sessions
   [start-workspace] ✓ Created terminal-1
   [start-workspace] ✓ Created terminal-2
   ...
   [start-workspace] ✓ Created terminal-7
   [launchAgentInTerminal] ✓ Agent will start in main workspace
   [launchAgentInTerminal] Sending "2" to accept warning
   [launchAgentInTerminal] ✓ Claude Code launched successfully
   ```

3. **Verify Terminal State** (via MCP)
   ```bash
   # Check application state
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
       // ... 6 more terminals
     ]
   }
   ```

4. **Verify tmux Sessions** (via MCP)
   ```bash
   # List all terminal sessions
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
- ✅ 7 terminals created (terminal-1 through terminal-7)
- ✅ All terminals in `idle` or `busy` status (not `error`)
- ✅ All terminals have valid `sessionId` (no `null` or missing)
- ✅ All terminals start in main workspace directory
- ✅ Claude Code running in all 7 terminals
- ✅ No "command not found" errors
- ✅ No "duplicate session" errors
- ✅ No stale error overlays visible in UI

### Phase 4: Factory Reset Test

**Goal**: Trigger factory reset and verify clean recovery

**Steps**:

1. **Trigger Factory Reset** (via MCP)
   ```bash
   # Reset workspace to defaults (does NOT auto-start)
   mcp__loom-ui__trigger_factory_reset

   # Wait for reset to complete (watch console logs)
   mcp__loom-ui__read_console_log
   ```

   **Expected log sequence**:
   ```
   [workspace-reset] Starting workspace reset
   [workspace-reset] Killing all loom tmux sessions
   [workspace-reset] ✓ Killed all sessions
   [workspace-reset] Destroying terminal session for terminal-1
   ...
   [workspace-reset] Destroying terminal session for terminal-7
   [workspace-reset] ✓ All terminals destroyed
   [workspace-reset] Resetting configuration to defaults
   [workspace-reset] ✓ Configuration reset complete
   ```

2. **Verify Clean State After Reset**
   ```bash
   # Check that all terminals are destroyed
   mcp__loom-ui__read_state_file

   # Verify no tmux sessions remain
   mcp__loom-terminals__list_terminals

   # Check for any errors in logs
   mcp__loom-logs__tail_daemon_log --lines=50
   ```

3. **Restart Workspace** (via MCP)
   ```bash
   # Start engine with reset configuration
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
   - **Target**: 100% (7/7 terminals)
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

**Last Updated**: 2025-10-16
**Status**: Active Testing
**Next Milestone**: 10 consecutive successful runs
