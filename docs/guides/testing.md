# Testing Strategy

## Daemon Integration Tests (Issue #13)

**Location**: `loom-daemon/tests/`

Comprehensive integration test suite for the daemon with 9 passing test cases:

**Test Infrastructure** (`tests/common/mod.rs`):
- `TestDaemon`: Manages isolated daemon instances with unique socket paths
- `TestClient`: Async IPC client with helper methods for all operations
- tmux helper functions for session management and cleanup
- Proper isolation with `#[serial]` attribute to prevent race conditions

**Test Coverage** (`tests/integration_basic.rs`):
1. Basic IPC (Ping/Pong, malformed JSON handling)
2. Terminal lifecycle (create, list, destroy)
3. Working directory support
4. Input handling
5. Multiple concurrent clients
6. Error conditions (non-existent terminals)

**Running Tests**:
```bash
npm run daemon:test                    # Run all daemon tests
npm run daemon:test:verbose           # With full output
cargo test --test integration_basic   # Run specific test file
```

**Key Implementation Details**:
- Daemon uses internally-tagged JSON: `{"type": "Ping"}`, `{"type": "CreateTerminal", "payload": {...}}`
- Tests use `LOOM_SOCKET_PATH` env var for isolation
- Each test spawns isolated daemon in temp directory
- Automatic cleanup on test completion

## Frontend Testing (Planned)

1. **Unit Tests**: Vitest for pure functions (state.ts, ui.ts)
2. **Integration Tests**: Playwright for E2E workflows
3. **Type Tests**: TypeScript strict mode as first line of defense

## MCP Testing and Instrumentation

**Location**: `mcp-loom/`, `.mcp.json`

Loom provides a unified MCP (Model Context Protocol) server that enables AI agents (including Claude Code) to inspect and interact with the running app for testing and debugging.

**Full API Documentation**: [docs/mcp/README.md](../mcp/README.md)

**Tool Categories**:
- **[Log Tools](../mcp/loom-logs.md)** - Daemon, Tauri, and terminal logs (4 tools)
- **[UI Tools](../mcp/loom-ui.md)** - UI interaction, console logs, workspace state (13 tools)
- **[Terminal Tools](../mcp/loom-terminals.md)** - Terminal management and IPC (17 tools)

### Console Logging to File

**Implementation**: `src/main.ts` (console interceptor) + `src-tauri/src/main.rs` (`append_to_console_log`)

All browser console output is automatically written to `~/.loom/console.log`:

```typescript
// Console interception (src/main.ts)
const originalConsoleLog = console.log;
console.log = (...args: unknown[]) => {
  originalConsoleLog(...args);  // Still log to DevTools
  writeToConsoleLog("INFO", ...args);  // Also write to file
};
```

**Log Format**:
```
[2025-10-15T05:05:06.088Z] [INFO] [launchAgentsForTerminals] Starting agent launch...
[2025-10-15T05:05:06.814Z] [INFO] [launchAgentInTerminal] Worktree setup complete
```

**Benefits**:
- Persistent logs survive app restarts
- AI agents can read logs via MCP to diagnose issues
- Debug output visible without watching DevTools in real-time
- Full visibility into factory reset and agent launch processes

### MCP Loom Server

**Package**: `mcp-loom/`
**Configuration**: `.mcp.json`

Unified MCP server providing tools for Claude Code to interact with Loom's state and logs:

**Available Tools**:

1. **`read_console_log`**
   - Reads browser console output from `~/.loom/console.log`
   - Returns recent log entries with timestamps
   - Use for debugging workspace start, agent launch, worktree setup

2. **`read_state_file`**
   - Reads current application state from `.loom/state.json`
   - Shows active terminals, session IDs, working directories
   - Use for verifying terminal creation and state management

3. **`read_config_file`**
   - Reads terminal configurations from `.loom/config.json`
   - Shows terminal roles, intervals, prompts
   - Use for verifying configuration persistence

4. **`trigger_start`**
   - Start engine with EXISTING config (shows confirmation dialog)
   - Uses current `.loom/config.json` to create terminals and launch agents
   - Does NOT reset or overwrite configuration
   - Use for restarting terminals after app restart or crash

5. **`trigger_force_start`**
   - Start engine with existing config WITHOUT confirmation
   - Same as trigger_start but bypasses confirmation prompt
   - Use for MCP automation and testing

6. **`trigger_factory_reset`**
   - Reset workspace to factory defaults (shows confirmation dialog)
   - Overwrites `.loom/config.json` with `defaults/config.json`
   - Does NOT auto-start the engine - must run trigger_start/force_start after
   - Use for resetting configuration to clean state

**MCP Configuration** (`.mcp.json`):
```json
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/Users/rwalters/GitHub/loom"
      }
    }
  }
}
```

**Usage Example** (from Claude Code):
```bash
# Read recent console logs to see workspace start progress
mcp__loom__read_console_log

# Check terminal state after start
mcp__loom__read_state_file

# Check terminal configuration
mcp__loom__read_config_file

# Start engine with existing config (bypasses confirmation for MCP automation)
mcp__loom__trigger_force_start

# Reset workspace to defaults (requires separate start command after)
mcp__loom__trigger_factory_reset
```

### Testing Workspace Start with MCP

**Goal**: Verify workspace start creates 7 terminals with Claude Code agents running autonomously in the main workspace

**Test Procedure**:

1. **Start Engine** (use force_start for MCP automation):
   ```bash
   mcp__loom__trigger_force_start
   ```

2. **Monitor Console Logs**:
   ```bash
   mcp__loom__read_console_log
   ```
   Look for:
   - `[start-workspace] Killing all loom tmux sessions`
   - `[start-workspace] ✓ Created terminal X`
   - `[launchAgentInTerminal] ✓ Agent will start in main workspace`
   - `[launchAgentInTerminal] Sending "2" to accept warning`

3. **Verify State**:
   ```bash
   mcp__loom__read_state_file
   ```
   Confirm 7 terminals exist with correct session IDs (no worktree paths yet)

4. **Verify Main Workspace** (agents start here, create worktrees on-demand):
   ```bash
   ls -la .loom/worktrees/
   # Should be empty or show only manually created worktrees
   # Agents will create .loom/worktrees/issue-{number} when claiming issues
   ```

**Expected Success Criteria**:
- ✅ 7 terminals created (terminal-1 through terminal-7)
- ✅ All terminals start in main workspace directory
- ✅ NO automatic worktrees created during startup
- ✅ Claude Code running in all 7 terminals (bypass permissions accepted)
- ✅ No "command not found" or "duplicate session" errors
- ✅ Console logs show successful agent launch sequence

**Note**: Agents now start in the main workspace and create worktrees on-demand using `pnpm worktree <issue>` when claiming GitHub issues. This prevents resource waste and provides semantic naming (`.loom/worktrees/issue-42` instead of `terminal-1`).

**Factory Reset + Start Workflow**:

To reset configuration AND start the engine:

```bash
# Step 1: Reset config to defaults (does NOT auto-start)
mcp__loom__trigger_factory_reset

# Step 2: Start engine with reset config
mcp__loom__trigger_force_start
```

### Debugging Common Issues

**Issue**: Commands concatenated in terminal output
- **Symptom**: `claude --dangerously-skip-permissions2` or multiple commands on one line
- **Check**: Console logs for timing of `send_terminal_input` calls
- **Fix**: Increase delay in `worktree-manager.ts` `sendCommand()` function

**Issue**: "duplicate session" errors
- **Symptom**: `fatal: duplicate session: loom-terminal-X`
- **Check**: tmux sessions before factory reset: `tmux -L loom list-sessions`
- **Fix**: `kill_all_loom_sessions` should run before creating terminals

**Issue**: Bypass permissions prompt not accepted
- **Symptom**: Terminals stuck at "WARNING: Claude Code running in Bypass Permissions mode"
- **Check**: Terminal output files for prompt appearance timing
- **Fix**: Adjust retry delays in `agent-launcher.ts`

**Issue**: Worktree creation fails
- **Symptom**: `fatal: '/path' already exists` or `is a missing but already registered worktree`
- **Check**: Existing worktrees: `git worktree list`
- **Fix**: Prune orphaned worktrees: `git worktree prune`

## Debugging

1. **State inspection**: Add `console.log(state.getTerminals())` in render
2. **TypeScript errors**: Run `pnpm exec tsc --noEmit`
3. **Hot reload**: Vite provides instant feedback on save
4. **Tauri DevTools**: Open with Cmd+Option+I in dev mode
