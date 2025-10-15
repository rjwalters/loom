# Implementation Checklist for Issue #96
## Separate Terminal Configuration IDs from Ephemeral Session IDs

### Phase 1: Core Infrastructure ✅ COMPLETED
- [x] Add `configId` field to Terminal interface
- [x] Update AppState to use configId as Map key
- [x] Add helper methods: getTerminalBySessionId(), getTerminalByConfigId()
- [x] Add migration logic in config.ts
- [x] Update defaults/config.json
- [x] Fix createPlainTerminal() to generate configId
- [x] Fix launchWorker() to generate configId

### Phase 2: Main.ts - Critical Session Management

**A. Workspace Initialization & Config Loading** (Lines 875-1029)
- [ ] `handleWorkspacePathInput()` - When loading config, create sessions for terminals with `id: "__needs_session__"`
- [ ] After creating session, update terminal.id with sessionId, keep configId stable
- [ ] `reconnectTerminals()` - Map daemon session IDs back to config IDs
- [ ] Update reconnect logic to use configId for state lookups, sessionId for daemon queries

**B. Terminal Creation Functions**
- [ ] `launchAgentsForTerminals()` - Uses terminal.id for IPC calls (correct), but needs configId for state updates (line 115)
- [ ] Factory reset logic (lines 280-432) - Create sessions and map to configIds properly

**C. Event Handlers & UI Interactions** (Lines 217-278, 1290-1596)
- [ ] `listen("close-terminal")` - Use configId for state.removeTerminal() (line 242)
- [ ] `listen("factory-reset-workspace")` - Map created sessionIds to configIds
- [ ] `startRename()` - Receives id from data attribute, convert to configId (line 383)
- [ ] `handleRecoverNewSession()` - Takes terminalId param, use configId for state operations (line 435)
- [ ] `handleRecoverAttachSession()` - Same as above (line 482)
- [ ] `handleAttachToSession()` - Same as above (line 498)
- [ ] `initializeTerminalDisplay()` - Should receive sessionId, look up terminal by it

**D. Drag & Drop State** (Lines 523-527, 1492-1596)
- [ ] `draggedTerminalId` - Should be draggedConfigId
- [ ] `dropTargetId` - Should be dropTargetConfigId
- [ ] All drag event handlers - Use configId from data attributes

**E. Output Poller Error Callback** (Lines 27-40)
- [ ] Currently finds terminal by sessionId, should use getTerminalBySessionId() helper
- [ ] Then use configId for state.updateTerminal()

### Phase 3: UI Layer (ui.ts)

**All data-terminal-id attributes must use configId:**
- [ ] `renderPrimaryTerminal()` - data-terminal-id should be configId
- [ ] `renderMiniTerminals()` - data-terminal-id should be configId
- [ ] `renderAvailableSessionsList()` - data-terminal-id should be configId
- [ ] All button data attributes (settings, clear, close, recovery) - use configId

**Display logic:**
- [ ] Terminal display still uses sessionId for xterm.js instances (correct)
- [ ] But UI lookups should use configId

### Phase 4: Terminal Manager (terminal-manager.ts)

**Decision: Keep using sessionId for xterm instances**

- [ ] Update method signatures to accept both configId and sessionId where needed
- [ ] Document that TerminalManager operates on sessionIds (ephemeral)
- [ ] Add comments explaining the sessionId usage

### Phase 5: Output Poller (output-poller.ts)

- [ ] Review and add comments that it operates on sessionIds
- [ ] Error callback - Use getTerminalBySessionId() to find configId
- [ ] Then use configId for state updates

### Phase 6: Autonomous Manager (autonomous-manager.ts)

- [ ] `startAutonomous(configId)` - Should use configId (persists across restarts)
- [ ] `stopAutonomous(configId)` - Should use configId
- [ ] `sendIntervalPrompt()` - Receives configId, looks up sessionId for IPC calls
- [ ] Interval tracking - Store by configId (survives daemon restarts)

### Phase 7: Terminal Settings Modal (terminal-settings-modal.ts)

- [ ] Review all state.updateTerminal() calls - Ensure using configId
- [ ] Form receives terminal object - has both configId and id

### Phase 8: Agent Launcher (agent-launcher.ts)

- [ ] Add comments that it operates on sessionIds for IPC
- [ ] Document that caller should use configId for state updates

### Phase 9: Worktree Manager (worktree-manager.ts)

- [ ] Add comments that it operates on sessionIds for terminal IPC
- [ ] Returns worktreePath which caller stores using configId

### Phase 10: Test Fixtures

**Fix all test files by adding configId:**
- [ ] `src/lib/state.test.ts` - Add configId to all Terminal fixtures (~20 instances)
- [ ] `src/lib/config.test.ts` - Add configId to all Terminal fixtures (~10 instances)
- [ ] `src/lib/autonomous-manager.test.ts` - Add configId to all Terminal fixtures (~30 instances)
- [ ] `src/lib/worktree-manager.test.ts` - Add configId if needed

### Phase 11: Integration & Testing

- [ ] Test migration from old config format
- [ ] Test workspace initialization with migrated config
- [ ] Test session recreation after daemon restart
- [ ] Test terminal creation (plain + worker)
- [ ] Test terminal renaming (configId stays same)
- [ ] Test terminal reordering (uses configId)
- [ ] Test autonomous mode (survives daemon restart)
- [ ] Run full CI checks
- [ ] Manual testing of all workflows

---

## Key Principles

### 1. configId (Stable, Persistent)
Used for:
- State management (AppState Map keys)
- UI data attributes (`data-terminal-id`)
- Config persistence (.loom/config.json)
- Autonomous manager tracking

### 2. id/sessionId (Ephemeral)
Used for:
- Daemon IPC calls (create_terminal, send_input, etc.)
- TerminalManager (xterm instances)
- OutputPoller (polling daemon output)
- Agent launcher IPC

### 3. Lookup Pattern
```typescript
// UI event receives configId from data attribute
const configId = element.getAttribute('data-terminal-id');
const terminal = state.getTerminalByConfigId(configId);

// Use sessionId for IPC
await invoke('some_terminal_command', { id: terminal.id });

// Use configId for state updates
state.updateTerminal(configId, { ... });
```

### 4. Migration Flow
```typescript
// Old config (no configId)
{ id: "uuid-123", name: "Shell" }

// After migration
{ configId: "terminal-1", id: "__needs_session__", name: "Shell" }

// After session creation
{ configId: "terminal-1", id: "new-uuid-456", name: "Shell" }
```

## Implementation Notes

### Critical Insight: Session Creation After Migration
After migrating old config format, terminals will have:
- `configId: "terminal-1"` (stable)
- `id: "__needs_session__"` (placeholder)

The `handleWorkspacePathInput()` function needs to:
1. Load config (which runs migration automatically)
2. Check if any terminals have `id: "__needs_session__"`
3. For those terminals, create daemon sessions
4. Update terminal.id with the new sessionId
5. Then proceed with reconnection/initialization

### Proposed Flow for Migrated Configs
```typescript
// In handleWorkspacePathInput(), after loading config:
const config = await loadConfig(); // Migration happens here
state.setNextAgentNumber(config.nextAgentNumber);

// NEW: Create sessions for migrated terminals
for (const agent of config.agents) {
  if (agent.id === "__needs_session__") {
    const sessionId = await invoke<string>("create_terminal", {
      name: agent.name,
      workingDir: expandedPath,
      role: agent.role || "default",
      instanceNumber: state.getNextAgentNumber(),
    });
    agent.id = sessionId; // Update sessionId, keep configId
    console.log(`Created session for migrated terminal ${agent.configId}: ${sessionId}`);
  }
}

// Now load agents into state
state.loadAgents(config.agents);

// And reconnect (which will now find the sessions we just created)
await reconnectTerminals();
```

##  Status
- Phase 1: ✅ Complete
- Phase 2-11: 🚧 In Progress - WIP commit made, needs systematic completion

Last updated: 2025-01-14
