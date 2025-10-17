# ADR-0003: Separate Configuration and State Files

## Status

Accepted

## Context

Loom needs to persist workspace-specific data across app restarts. Two types of data exist:

1. **Configuration**: User preferences, terminal roles, intervals (should persist)
2. **Runtime State**: Active terminal sessions, process IDs, connection status (ephemeral)

The question was whether to:
- Store everything in one file
- Split into configuration (committed) and state (gitignored)
- Split into configuration (gitignored) and state (gitignored)

## Decision

Split into **two separate files, both gitignored**:

1. **`.loom/config.json`** - User configuration that persists across restarts
   - Terminal roles and names
   - Autonomous intervals and prompts
   - Next agent number (monotonic counter)
   - Workspace preferences

2. **`.loom/state.json`** - Runtime state that changes during operation
   - Active tmux session IDs
   - Terminal working directories
   - Connection status
   - Process IDs

Both files are gitignored - each developer has their own workspace setup.

## Consequences

### Positive

- **Clear separation of concerns**: Config = what user sets, State = what app tracks
- **Safe restarts**: Can reload config without conflicting runtime state
- **No git conflicts**: Each developer has independent workspace configuration
- **Factory reset friendly**: Can reset config without affecting running state
- **Easier debugging**: Separate files make it clear what persisted vs runtime
- **Schema evolution**: Can version config independently of state

### Negative

- **Two files to manage**: More complexity than single file
- **Sync challenges**: Config and state must stay coordinated
- **Migration complexity**: Schema changes affect two files
- **Potential inconsistency**: Config and state could drift if not careful

## Alternatives Considered

### 1. Single File (Committed to Git)

**Rejected because**:
- Creates git conflicts when multiple developers work on same repo
- Exposes personal preferences (agent names, intervals) to all developers
- Can't have developer-specific workspace setups
- Security risk if session IDs or PIDs committed

### 2. Single File (Gitignored)

**Rejected because**:
- Harder to distinguish what should persist vs what's ephemeral
- Factory reset would wipe runtime state unnecessarily
- Debugging more difficult (mixed concerns)
- No clear schema for "what should persist"

### 3. Database (SQLite)

**Rejected because**:
- Overkill for simple key-value data
- Harder to inspect and debug
- More dependencies
- JSON files simpler for text-based tooling

### 4. Multiple Config Files per Terminal

**Rejected because**:
- Harder to maintain global config (e.g., next agent number)
- More files to manage
- No clear benefit over single config file

## Implementation Details

**Config Structure** (`.loom/config.json`):
```json
{
  "nextAgentNumber": 4,
  "agents": [
    {
      "id": "1",
      "name": "Shell",
      "role": "default",
      "roleConfig": { ... }
    }
  ]
}
```

**State Structure** (`.loom/state.json`):
```json
{
  "terminals": [
    {
      "id": "terminal-1",
      "sessionId": "loom-terminal-1",
      "workingDirectory": "/path/to/workspace",
      "status": "connected"
    }
  ]
}
```

## References

- Implementation: `src/lib/config.ts`, `src/lib/state.ts`
- Related: ADR-0001 (Observer Pattern)
- `.gitignore` line 33-34: `.loom/` directory exclusion
- Issue #6: .loom/ directory configuration
