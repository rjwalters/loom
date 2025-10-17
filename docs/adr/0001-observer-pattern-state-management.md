# ADR-0001: Observer Pattern for State Management

## Status

Accepted

## Context

Loom needed a state management solution for coordinating terminal data across multiple UI components (header, primary terminal view, mini terminal row). The state needed to:

- Be the single source of truth for all terminal data
- Notify UI components when data changes
- Support multiple listeners without tight coupling
- Scale to 100+ terminals with O(1) lookups
- Work well with vanilla TypeScript (no frameworks)

## Decision

Implement an Observer Pattern using a `Map`-based store with listener callbacks:

```typescript
export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private listeners: Set<() => void> = new Set();

  private notify(): void {
    this.listeners.forEach(cb => cb());
  }

  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }
}
```

Key architectural choices:
- **Map storage**: O(1) terminal lookups by ID
- **Set of listeners**: Efficient callback management
- **Cleanup functions**: `onChange()` returns unsubscribe function
- **Strong typing**: `Terminal` interface and `TerminalStatus` enum
- **Safety constraints**: Cannot remove last terminal, auto-promote primary

## Consequences

### Positive

- **Decoupled architecture**: State doesn't know about UI implementation
- **Single source of truth**: No conflicting data across components
- **Automatic UI updates**: Change state once, all listeners update
- **Easy to extend**: Add new listeners without modifying existing code
- **Testable**: State logic can be tested independently of UI
- **Performance**: Map provides O(1) lookups, Set provides efficient iteration

### Negative

- **Manual listener management**: Developers must remember to call cleanup functions
- **No built-in devtools**: Unlike Redux/Zustand (but simpler for our use case)
- **Potential memory leaks**: If cleanup functions not called properly
- **No middleware pattern**: Can't intercept state changes (not needed yet)

## Alternatives Considered

### 1. Redux

**Rejected** because:
- Too heavy for vanilla TypeScript project
- Boilerplate overhead (actions, reducers, middleware)
- Overkill for our relatively simple state needs
- Wanted to demonstrate fundamental patterns

### 2. Direct DOM Manipulation

**Rejected** because:
- Tight coupling between state and UI
- Hard to test
- Difficult to add new views
- No single source of truth

### 3. Event Emitter Pattern

**Rejected** because:
- String-based event names (no type safety)
- More complex API than needed
- Observer pattern simpler for our use case

### 4. MobX/Immer

**Rejected** because:
- Proxy-based reactivity adds complexity
- Not as instructive for learning fundamentals
- Wanted explicit state changes

## References

- Implementation: `src/lib/state.ts`
- Usage: `src/main.ts` (render cycle)
- Related: ADR-0002 (Vanilla TypeScript choice)
- Related: ADR-0003 (Config/State file split)
