# Issue: False Positive "Terminal Session Missing" Error Overlays

## Summary

Terminals display "Terminal Session Missing" error overlays even when tmux sessions exist and are functioning correctly. This is a false positive caused by stale state persistence and race conditions in the session health check flow.

## Current Status

- **Observed**: Multiple terminals show error overlay despite tmux sessions being active
- **Root Causes Identified**: Multiple contributing factors (detailed below)
- **Severity**: High - Prevents users from accessing working terminals

## Root Causes Identified

### 1. Stale State Persistence

**Location**: `.loom/state.json`

When terminals are marked with `status: "error"` and `missingSession: true`, this state persists to disk. On app restart, the stale flags are loaded before any session health checks run.

**Evidence**:
```json
// Example from .loom/state.json
{
  "terminals": [
    {"id": "terminal-1", "status": "error", "missingSession": true, ...},
    {"id": "terminal-2", "status": "error", "missingSession": true, ...}
  ]
}
```

### 2. Race Condition: Render Before Health Check Completes

**Location**: `src/main.ts` lines 114-120, 209-234

The app has a timer that calls `render()` every second to update busy/idle time displays. The render function calls `renderPrimaryTerminal()`, which checks the `missingSession` flag and immediately renders the error overlay if true.

Meanwhile, `initializeTerminalDisplay()` performs an async session health check that takes time to complete. The render loop shows the error overlay based on stale state BEFORE the health check finishes.

**Code Flow**:
```typescript
// Every second
setInterval(() => {
  render();  // Calls renderPrimaryTerminal()
}, 1000);

// renderPrimaryTerminal checks the flag synchronously
const hasMissingSession =
  terminal.status === TerminalStatus.Error && terminal.missingSession === true;
if (hasMissingSession) {
  // Shows error immediately, doesn't wait for health check
  setTimeout(() => renderMissingSessionError(terminal.id, terminal.id), 0);
}

// Meanwhile, initializeTerminalDisplay runs async check
async function initializeTerminalDisplay(terminalId: string) {
  const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
  // By the time this completes, error overlay is already showing
}
```

**Debug Logging Evidence**:
```
[2025-10-16T09:29:11.268Z] [INFO] [renderPrimaryTerminal] Terminal terminal-6 has missingSession=true, will render error overlay
[2025-10-16T09:29:11.269Z] [INFO] [initializeTerminalDisplay] Checking session health for terminal terminal-6...
[2025-10-16T09:29:11.272Z] [INFO] [renderMissingSessionError] Rendering error overlay for terminal terminal-6
```

Notice: Error overlay is rendered at 11.272Z, but we only started checking health at 11.269Z - only 3ms later!

### 3. Daemon Connection Issues During Startup

**Location**: `src-tauri/src/daemon_client.rs` line 90

When the app starts before the daemon is ready, or when the daemon is restarted independently, the `UnixStream::connect()` call fails with "Connection refused (os error 61)".

**Evidence from logs**:
```
[2025-10-16T09:23:23.873Z] [ERROR] [initializeTerminalDisplay] Failed to check session health: Connection refused (os error 61)
```

This causes the catch block to run with "Continue anyway - better to try than not", but terminals already have `missingSession: true` from stale state, so the error overlay persists.

### 4. Original Bug: `has_tmux_session()` Required Terminal Registration

**Location**: `loom-daemon/src/terminal.rs` lines 351-363 (OLD CODE - now fixed)

**Original Problem**:
```rust
pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
    let info = self
        .terminals
        .get(id)
        .ok_or_else(|| anyhow!("Terminal not found"))?;  // ❌ Failed here

    let output = Command::new("tmux")
        .args(["-L", "loom"])
        .args(["has-session", "-t", &info.tmux_session])
        .output()?;

    Ok(output.status.success())
}
```

This returned `Err("Terminal not found")` when the frontend created state before the daemon registered the terminal, causing a race condition.

**Fix Applied**:
```rust
pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
    // First check if we have this terminal registered
    if let Some(info) = self.terminals.get(id) {
        // Terminal is registered - check its specific tmux session
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", &info.tmux_session])
            .output()?;
        return Ok(output.status.success());
    }

    // Terminal not registered yet - check if ANY loom session with this ID exists
    // This handles the race condition where frontend creates state before daemon registers
    log::debug!("Terminal {id} not found in registry, checking tmux sessions directly");

    let output = Command::new("tmux")
        .args(["-L", "loom"])
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;

    if !output.status.success() {
        // tmux server not running or no sessions
        return Ok(false);
    }

    let sessions = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("loom-{id}-");

    // Check if any session matches our terminal ID prefix
    let has_session = sessions.lines().any(|s| s.starts_with(&prefix));

    log::debug!(
        "Terminal {id} tmux session check (unregistered): {}",
        if has_session { "found" } else { "not found" }
    );

    Ok(has_session)
}
```

This fix allows the daemon to check tmux sessions even when the terminal isn't registered yet.

## Testing Observations

### Successful Direct Daemon Test

```bash
$ echo '{"type":"CheckSessionHealth","payload":{"id":"terminal-1"}}' | nc -U ~/.loom/daemon.sock
{"type":"SessionHealth","payload":{"has_session":true}}
```

✅ The daemon correctly reports session exists when tested directly.

### Tmux Sessions Confirmed Active

```bash
$ tmux -L loom list-sessions
loom-terminal-1-claude-code-worker-28: 1 windows (created...)
loom-terminal-2-claude-code-worker-29: 1 windows (created...)
...
```

✅ All tmux sessions exist and Claude Code is actively running in them.

### Terminal Output Files Show Activity

```bash
$ tail -5 /tmp/loom-terminal-1.out
Preparing to list loom issues with label loom:ready
[Continuing with GitHub API calls...]
```

✅ Terminal output shows Claude Code is working normally.

### UI Shows False Positive

Despite all the above, the UI displays:
```
❌ Terminal Session Missing

The tmux session for this terminal no longer exists.
This can happen if the daemon was restarted or the session was killed.
```

❌ This is a false positive - the session exists and is working.

## Debug Logging Added

To diagnose this issue, we added comprehensive debug logging:

### Frontend (`src/main.ts`)

```typescript
// Check session health before initializing
try {
  console.log(`[initializeTerminalDisplay] Checking session health for terminal ${terminalId}...`);
  const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
  console.log(`[initializeTerminalDisplay] check_session_health returned: ${hasSession} for terminal ${terminalId}`);

  if (!hasSession) {
    console.warn(`[initializeTerminalDisplay] Terminal ${terminalId} has no tmux session`);
    // Mark terminal as having missing session
    const terminal = state.getTerminal(terminalId);
    console.log(`[initializeTerminalDisplay] Terminal state before update:`, terminal);
    if (terminal && !terminal.missingSession) {
      console.log(`[initializeTerminalDisplay] Setting missingSession=true for terminal ${terminalId}`);
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Error,
        missingSession: true,
      });
    }
    return;
  }

  console.log(`[initializeTerminalDisplay] Session health check passed for terminal ${terminalId}, proceeding with xterm initialization`);
} catch (error) {
  console.error(`[initializeTerminalDisplay] Failed to check session health:`, error);
}
```

### Frontend UI (`src/lib/ui.ts`)

```typescript
// If missing session, render error UI inside the content container after DOM update
if (hasMissingSession) {
  console.log(`[renderPrimaryTerminal] Terminal ${terminal.id} has missingSession=true, will render error overlay`);
  setTimeout(() => {
    renderMissingSessionError(terminal.id, terminal.id);
  }, 0);
} else {
  console.log(`[renderPrimaryTerminal] Terminal ${terminal.id} has missingSession=${terminal.missingSession}, will show xterm`);
}

export function renderMissingSessionError(sessionId: string, configId: string): void {
  console.log(`[renderMissingSessionError] Rendering error overlay for terminal ${sessionId}`);
  const container = document.getElementById(`xterm-container-${sessionId}`);
  if (!container) {
    console.warn(`[renderMissingSessionError] Container #xterm-container-${sessionId} not found!`);
    return;
  }
  console.log(`[renderMissingSessionError] Found container, replacing with error UI`);
  // ...render error overlay...
}
```

### Backend (`loom-daemon/src/terminal.rs`)

```rust
pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
    // First check if we have this terminal registered
    if let Some(info) = self.terminals.get(id) {
        // Terminal is registered - check its specific tmux session
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", &info.tmux_session])
            .output()?;
        return Ok(output.status.success());
    }

    // Terminal not registered yet - check if ANY loom session with this ID exists
    log::debug!("Terminal {id} not found in registry, checking tmux sessions directly");

    let output = Command::new("tmux")
        .args(["-L", "loom"])
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;

    if !output.status.success() {
        return Ok(false);
    }

    let sessions = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("loom-{id}-");

    let has_session = sessions.lines().any(|s| s.starts_with(&prefix));

    log::debug!(
        "Terminal {id} tmux session check (unregistered): {}",
        if has_session { "found" } else { "not found" }
    );

    Ok(has_session)
}
```

## Proposed Fixes

### Fix 1: Clear Stale State on Startup (Quick Fix)

**Location**: `src/main.ts` - `initializeApp()` function

Before loading terminals from state, check if sessions actually exist and clear stale flags:

```typescript
async function initializeApp() {
  // ... existing code ...

  const config = await loadWorkspaceConfig();

  // Clear stale missingSession flags before loading
  for (const agent of config.agents) {
    if (agent.missingSession) {
      try {
        const hasSession = await invoke<boolean>("check_session_health", { id: agent.id });
        if (hasSession) {
          // Session exists - clear the stale flag
          agent.missingSession = undefined;
          agent.status = TerminalStatus.Idle;
        }
      } catch (error) {
        console.warn(`Failed to verify session for ${agent.id}, keeping error state`);
      }
    }
  }

  state.loadAgents(config.agents);
  // ...
}
```

**Pros**: Simple, fixes the immediate symptom
**Cons**: Adds startup delay, doesn't fix the render race condition

### Fix 2: Don't Persist missingSession Flag (Better)

**Location**: `src/lib/config.ts` - `saveState()` function

The `missingSession` flag is a runtime status indicator, not configuration. It shouldn't be persisted to disk:

```typescript
export async function saveState(data: StateFile): Promise<void> {
  // ... existing code ...

  // Strip runtime-only flags before saving
  const sanitizedTerminals = data.terminals.map(t => {
    const { missingSession, ...rest } = t;
    return rest;
  });

  const sanitized = {
    ...data,
    terminals: sanitizedTerminals
  };

  await writeTextFile(statePath, JSON.stringify(sanitized, null, 2), {
    dir: BaseDirectory.Home,
  });
}
```

**Pros**: Prevents stale flags from persisting across restarts
**Cons**: Doesn't fix the render race condition during runtime

### Fix 3: Debounce Session Health Checks (Defensive)

**Location**: `src/main.ts` - `initializeTerminalDisplay()` function

Don't call session health check on every render. Only check when terminal is first created or when explicitly requested:

```typescript
// Track which terminals have been checked
const healthCheckedTerminals = new Set<string>();

async function initializeTerminalDisplay(terminalId: string) {
  // Skip placeholder IDs
  if (terminalId === "__unassigned__") {
    return;
  }

  // Only check health once per terminal
  if (!healthCheckedTerminals.has(terminalId)) {
    try {
      console.log(`[initializeTerminalDisplay] Checking session health for terminal ${terminalId}...`);
      const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
      console.log(`[initializeTerminalDisplay] check_session_health returned: ${hasSession}`);

      healthCheckedTerminals.add(terminalId);

      if (!hasSession) {
        const terminal = state.getTerminal(terminalId);
        if (terminal && !terminal.missingSession) {
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        }
        return;
      }

      // Session exists - clear any stale flags
      const terminal = state.getTerminal(terminalId);
      if (terminal && terminal.missingSession) {
        console.log(`[initializeTerminalDisplay] Clearing stale missingSession flag`);
        state.updateTerminal(terminal.id, {
          status: TerminalStatus.Idle,
          missingSession: undefined,
        });
      }
    } catch (error) {
      console.error(`[initializeTerminalDisplay] Failed to check session health:`, error);
    }
  }

  // ... rest of function (create/show xterm) ...
}
```

**Pros**: Prevents redundant checks, clears stale flags after successful check
**Cons**: Doesn't recheck if session actually dies later

### Fix 4: Separate Health Check Phase (Best)

**Location**: `src/main.ts` - new `verifyTerminalSessions()` function

Run session health checks BEFORE rendering any terminals, and wait for all checks to complete:

```typescript
async function verifyTerminalSessions(terminals: Terminal[]): Promise<void> {
  console.log("[verifyTerminalSessions] Checking health of all terminals...");

  const checks = terminals.map(async (terminal) => {
    if (terminal.missingSession) {
      try {
        const hasSession = await invoke<boolean>("check_session_health", {
          id: terminal.id
        });

        if (hasSession) {
          // Clear stale flag
          console.log(`[verifyTerminalSessions] Clearing stale flag for ${terminal.id}`);
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        } else {
          console.warn(`[verifyTerminalSessions] Confirmed missing for ${terminal.id}`);
        }
      } catch (error) {
        console.error(`[verifyTerminalSessions] Check failed for ${terminal.id}:`, error);
      }
    }
  });

  await Promise.all(checks);
  console.log("[verifyTerminalSessions] All checks complete");
}

async function initializeApp() {
  // ... load config ...
  state.loadAgents(config.agents);

  // Verify sessions before rendering
  await verifyTerminalSessions(state.getTerminals());

  // NOW start rendering
  render();
  // ...
}
```

**Pros**: Clean separation of concerns, no race conditions
**Cons**: Adds startup delay (but only once, not per render)

## Recommended Solution

**Combination of Fixes 2, 3, and 4:**

1. **Fix 2**: Don't persist `missingSession` flag to disk (prevents stale state)
2. **Fix 3**: Debounce health checks with Set tracking (prevents redundant checks)
3. **Fix 4**: Run verification phase before first render (eliminates race condition)

This provides defense-in-depth:
- Fix 2 prevents the problem from persisting across restarts
- Fix 3 prevents redundant checks during runtime
- Fix 4 ensures clean state before any rendering

## Additional Debugging Strategies

### Strategy 1: MCP-Based Session Health Dashboard

Create an MCP tool to query session health for all terminals and compare with tmux reality:

```typescript
// Add to mcp-loom-ui
export async function checkAllSessionHealth() {
  const stateFile = await readStateFile();
  const tmuxSessions = await listTmuxSessions();

  const report = stateFile.terminals.map(t => ({
    id: t.id,
    name: t.name,
    stateStatus: t.status,
    stateMissing: t.missingSession,
    tmuxExists: tmuxSessions.some(s => s.startsWith(`loom-${t.id}-`)),
    daemonRegistered: await checkDaemonRegistry(t.id),
  }));

  return report;
}
```

This would let us quickly see the discrepancy between state, daemon registry, and tmux reality.

### Strategy 2: Frontend State Inspector

Add a keyboard shortcut (e.g., Cmd+Shift+D) to dump current state to console:

```typescript
document.addEventListener('keydown', (e) => {
  if (e.metaKey && e.shiftKey && e.key === 'D') {
    console.log("=== STATE DUMP ===");
    console.log("Terminals:", state.getTerminals());
    console.log("Health checked:", healthCheckedTerminals);
    console.log("Output poller status:", outputPoller.getStatus());
    console.log("Health monitor:", healthMonitor.getHealth());
  }
});
```

### Strategy 3: Session Health Test Command

Add a slash command `/test-sessions` that verifies all sessions and reports mismatches:

```typescript
// In slash commands
async function testSessions() {
  const terminals = state.getTerminals();
  const results = [];

  for (const terminal of terminals) {
    const hasSession = await invoke<boolean>("check_session_health", {
      id: terminal.id
    });
    const stateThinks = !terminal.missingSession;

    if (hasSession !== stateThinks) {
      results.push({
        terminal: terminal.name,
        id: terminal.id,
        tmuxExists: hasSession,
        stateSays: stateThinks ? "exists" : "missing",
        mismatch: true
      });
    }
  }

  console.table(results);
  return results;
}
```

### Strategy 4: Continuous Health Monitoring

Instead of checking once, periodically verify session health and auto-clear false positives:

```typescript
// Run every 30 seconds
setInterval(async () => {
  const terminals = state.getTerminals();

  for (const terminal of terminals) {
    if (terminal.missingSession) {
      try {
        const hasSession = await invoke<boolean>("check_session_health", {
          id: terminal.id
        });

        if (hasSession) {
          console.log(`[health-monitor] Auto-clearing false positive for ${terminal.id}`);
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        }
      } catch (error) {
        // Ignore - we'll try again in 30s
      }
    }
  }
}, 30000);
```

**Pros**: Self-healing, catches false positives automatically
**Cons**: Adds background load

## Files Modified

### Already Modified
- `loom-daemon/src/terminal.rs` - Fixed `has_tmux_session()` to handle unregistered terminals
- `src/main.ts` - Added debug logging
- `src/lib/ui.ts` - Added debug logging

### Need Modification
- `src/lib/config.ts` - Strip `missingSession` from persisted state (Fix 2)
- `src/main.ts` - Add session verification phase (Fix 4)
- `src/main.ts` - Add health check debouncing (Fix 3)

## Testing Plan

### Test 1: Fresh Start with Clean State

1. Delete `.loom/state.json`
2. Start app via `pnpm app:preview`
3. Verify no terminals show error overlay
4. Verify all terminals show xterm with Claude Code running

**Expected**: ✅ No false positives

### Test 2: Restart with Existing State

1. Start app, wait for terminals to load
2. Quit app (keep daemon running)
3. Restart app
4. Verify terminals don't show error overlay

**Expected**: ✅ No false positives from stale state

### Test 3: Daemon Restart Scenario

1. Start app with terminals running
2. Kill daemon: `pkill loom-daemon`
3. Restart daemon: `pnpm daemon:preview`
4. Switch between terminals in UI
5. Verify error overlays appear (correct behavior)
6. Use "Create New Session" button
7. Verify new session works

**Expected**: ✅ Error shown when truly missing, recovery works

### Test 4: Race Condition Test

1. Start app
2. Click through terminals rapidly
3. Verify no error overlays flash during switches

**Expected**: ✅ No transient false positives during rapid switching

## Open Questions

1. **Should we recheck session health periodically?**
   - Pro: Catches sessions that die during runtime
   - Con: Adds background load
   - Recommendation: Yes, but with long interval (30-60s)

2. **Should missingSession be persisted at all?**
   - Current: Yes (causes stale state)
   - Alternative: Make it purely runtime status
   - Recommendation: Don't persist it

3. **What if daemon connection fails during health check?**
   - Current: Catch block continues, shows stale state
   - Alternative: Show "Connecting to daemon..." state
   - Recommendation: Add intermediate "checking" state

4. **Should we block rendering until health checks complete?**
   - Current: No (causes race condition)
   - Alternative: Block with loading spinner
   - Recommendation: Yes, but only on initial load

## Related Issues

- Issue #209: UTF-8 character boundary panic (Fixed - merged)
- Issue #211: AgentOutput implementation (Complete - merged)
- Issue #148: Command execution ordering (Pre-existing test failures)

## References

- Console logs: `~/.loom/console.log`
- Daemon logs: `~/.loom/daemon.log`
- Terminal output: `/tmp/loom-terminal-*.out`
- State file: `.loom/state.json`
- Config file: `.loom/config.json`
