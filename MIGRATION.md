# Interval Prompt Manager Migration Guide

## Overview

This document outlines the migration from `autonomous-manager.ts` (fixed-interval timers) to `interval-prompt-manager.ts` (responsive min-time-between semantics).

## Implementation Status

### ✅ Completed
- [x] Created `interval-prompt-manager.ts` with responsive semantics
- [x] Implemented TerminalIntervalState tracking
- [x] Added 10-second periodic state polling
- [x] Implemented idle/busy state detection via `terminal-state-parser.ts`
- [x] Added immediate prompt trigger on idle + min-time elapsed
- [x] Support for interval 0 (continuous loop)
- [x] Overrun protection (prevents overlapping prompts)

### 🔄 In Progress
- [ ] Replace autonomous-manager usage across codebase
- [ ] Update UI to show next prompt timing
- [ ] Test scenarios (interval 0, 1min, 5min)

### 📋 TODO
- [ ] Integration testing with real Loom app
- [ ] Performance testing (5 terminals, 1 hour)
- [ ] Update documentation
- [ ] Deprecate autonomous-manager.ts

## Migration Strategy

### Phase 1: Core Replacement

Replace all `getAutonomousManager()` calls with `getIntervalPromptManager()`:

**Files to update:**
1. `src/main.ts` - Cleanup on window unload
2. `src/lib/workspace-lifecycle.ts` - Start all on workspace init
3. `src/lib/terminal-actions.ts` - Terminal start/stop/restart
4. `src/lib/terminal-settings-modal.ts` - Settings changes

**Pattern:**
```typescript
// OLD
import { getAutonomousManager } from "./lib/autonomous-manager";
const autonomousManager = getAutonomousManager();
autonomousManager.startAutonomous(terminal);

// NEW
import { getIntervalPromptManager } from "./lib/interval-prompt-manager";
const intervalManager = getIntervalPromptManager();
intervalManager.start(terminal);
```

### Phase 2: Method Mapping

| Old Method | New Method | Notes |
|------------|------------|-------|
| `startAutonomous(terminal)` | `start(terminal)` | Same signature |
| `stopAutonomous(terminalId)` | `stop(terminalId)` | Now synchronous (no await needed) |
| `isAutonomous(terminalId)` | `isManaged(terminalId)` | Renamed for clarity |
| `restartAutonomous(terminal)` | `restart(terminal)` | Same signature |
| `startAllAutonomous(state)` | `startAll(state)` | Same signature |
| `stopAll()` | `stopAll()` | Now synchronous |
| `getStatus(terminalId)` | `getStatus(terminalId)` | Different return type |
| `getAllStatus()` | `getAllStatus()` | Different return type |
| `runNow(terminal)` | `runNow(terminal)` | Same signature |

### Phase 3: Type Updates

Update type references:

```typescript
// OLD
import type { AutonomousInterval } from "./lib/autonomous-manager";

// NEW
import type { TerminalIntervalState } from "./lib/interval-prompt-manager";
```

**Key differences:**
- `AutonomousInterval.intervalId` → Removed (no longer needed)
- `AutonomousInterval.targetInterval` → `TerminalIntervalState.minInterval`
- `AutonomousInterval.lastRun` → `TerminalIntervalState.lastPromptTime`
- Added: `TerminalIntervalState.isIdle`
- Added: `TerminalIntervalState.previousStatus`
- Added: `TerminalIntervalState.intervalPrompt`

## Detailed Migration Steps

### Step 1: Update main.ts

**Location**: `src/main.ts` (cleanup on window unload)

```typescript
// Find: line ~472
const { getAutonomousManager } = await import("./lib/autonomous-manager");
const autonomousManager = getAutonomousManager();
await autonomousManager.stopAll();

// Replace with:
const { getIntervalPromptManager } = await import("./lib/interval-prompt-manager");
const intervalManager = getIntervalPromptManager();
intervalManager.stopAll(); // No longer async
```

### Step 2: Update workspace-lifecycle.ts

**Location**: `src/lib/workspace-lifecycle.ts` (start all on init)

```typescript
// Find usages of getAutonomousManager
// Replace with getIntervalPromptManager

// Example:
const { getAutonomousManager } = await import("./autonomous-manager");
getAutonomousManager().startAllAutonomous(dependencies.state);

// Becomes:
const { getIntervalPromptManager } = await import("./interval-prompt-manager");
getIntervalPromptManager().startAll(dependencies.state);
```

### Step 3: Update terminal-actions.ts

**Location**: `src/lib/terminal-actions.ts`

Multiple usages for start/stop/restart terminals:

```typescript
// Pattern 1: Starting terminal
getAutonomousManager().startAutonomous(terminal);
// Becomes:
getIntervalPromptManager().start(terminal);

// Pattern 2: Stopping terminal
await getAutonomousManager().stopAutonomous(terminalId);
// Becomes:
getIntervalPromptManager().stop(terminalId);

// Pattern 3: Checking status
if (getAutonomousManager().isAutonomous(terminalId)) { ... }
// Becomes:
if (getIntervalPromptManager().isManaged(terminalId)) { ... }
```

### Step 4: Update terminal-settings-modal.ts

**Location**: `src/lib/terminal-settings-modal.ts`

Update interval restart on settings change:

```typescript
// Find restart calls
await getAutonomousManager().restartAutonomous(terminal);

// Replace with:
getIntervalPromptManager().restart(terminal);
```

### Step 5: Update Tests

**Files:**
- `src/lib/autonomous-manager.test.ts` → Rename to `interval-prompt-manager.test.ts`
- `src/lib/terminal-actions.test.ts` → Update mocks

Update test imports and assertions to match new API.

## Testing Checklist

### Unit Tests
- [ ] Test min-time-between semantics
- [ ] Test interval 0 (continuous loop)
- [ ] Test idle detection
- [ ] Test state transitions (busy→idle)
- [ ] Test overrun protection
- [ ] Test manual runNow()

### Integration Tests
- [ ] Start workspace with 5 terminals
- [ ] Verify state polling starts
- [ ] Change terminal interval in settings
- [ ] Stop and restart terminals
- [ ] Window unload cleanup

### Manual Test Scenarios

**Scenario 1: Fast turnaround (interval: 60000 = 1 min)**
1. Set Builder interval to 60000ms (1 min)
2. Create loom:issue that completes in 30 seconds
3. Expected: Prompt sent 30 seconds after idle (1 min since last prompt)
4. Verify: Agent starts next task immediately

**Scenario 2: Continuous loop (interval: 0)**
1. Set Builder interval to 0
2. Add multiple `loom:issue` issues
3. Expected: Builder claims issues continuously without delays
4. Verify: No prompt spam (waits for idle between prompts)
5. Monitor: Check no performance degradation

**Scenario 3: Long-running task (interval: 300000 = 5 min)**
1. Set Judge interval to 300000ms (5 min)
2. Start review that takes 10 minutes
3. Expected: No prompt while busy
4. Expected: Prompt sent immediately when idle (10 min elapsed)

**Scenario 4: Manual override**
1. Set interval to 300000ms (5 min)
2. Agent idle, last prompt 2 minutes ago
3. Click "Run Now" in UI
4. Expected: Agent starts working immediately
5. Expected: Next auto-prompt in 5 min from manual trigger

### Performance Testing
- [ ] 5 terminals with mixed intervals (0, 60000, 300000)
- [ ] Run for 1 hour continuously
- [ ] Monitor CPU usage (should be minimal - just 10s polls)
- [ ] Monitor memory usage (should be stable)
- [ ] Check no polling leaks (verify cleanup on stop)

## Behavioral Changes

### What's Different

**Before (autonomous-manager):**
- Fixed-interval timers fire regardless of agent state
- If busy, skip execution (wasted timer tick)
- Agent idle between tasks waiting for next timer
- Inefficient: ~40% idle time with 3-5 min intervals

**After (interval-prompt-manager):**
- State-aware polling checks agent idle/busy
- Prompts trigger when idle + min-time elapsed
- Agent starts next task immediately after idle + min-time
- Efficient: ~5-10% idle time
- Interval 0: Back-to-back task execution

### What's the Same

- API surface mostly compatible (same method names)
- Configuration unchanged (still uses `targetInterval`, `intervalPrompt`)
- Overrun protection (no overlapping prompts)
- Manual "Run Now" functionality

## Rollback Plan

If issues discovered:
1. Revert changes to use `autonomous-manager.ts`
2. Keep `interval-prompt-manager.ts` for future iteration
3. File bugs for specific scenarios that fail
4. Re-plan migration with additional safeguards

## Future Enhancements

Once stable, consider:
- [ ] UI showing "Next prompt in: 2m 30s"
- [ ] UI indicating continuous loop mode (interval: 0)
- [ ] Pause/resume individual terminals
- [ ] Metrics: average idle time per terminal
- [ ] Adaptive intervals based on task duration

## References

- Issue #744
- `src/lib/interval-prompt-manager.ts` - New implementation
- `src/lib/autonomous-manager.ts` - Original implementation
- `src/lib/terminal-state-parser.ts` - State detection
