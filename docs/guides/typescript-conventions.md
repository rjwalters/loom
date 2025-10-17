# TypeScript Conventions

## Strict Mode

`tsconfig.json` has strict mode enabled:
- `strict: true` - All strict checks
- `noUnusedLocals: true` - No unused variables
- `noUnusedParameters: true` - No unused function parameters
- `noFallthroughCasesInSwitch: true` - Explicit breaks in switch

## Type Safety Patterns

### 1. Enums for Fixed Sets

```typescript
export enum TerminalStatus {
  Idle = 'idle',
  Busy = 'busy',
  NeedsInput = 'needs_input',
  Error = 'error',
  Stopped = 'stopped'
}
```

### 2. Interfaces for Data Structures

```typescript
export interface Terminal {
  id: string;
  name: string;
  status: TerminalStatus;
  isPrimary: boolean;
}
```

### 3. Return Types for Cleanup

```typescript
onChange(callback: () => void): () => void {
  this.listeners.add(callback);
  return () => this.listeners.delete(callback); // Cleanup function
}
```

## Common Pitfalls

### 1. Forgetting to Call notify()

When adding a new state mutation method, always call `this.notify()`:

```typescript
updateTerminalStatus(id: string, status: TerminalStatus): void {
  const terminal = this.terminals.get(id);
  if (terminal) {
    terminal.status = status;
    this.notify(); // DON'T FORGET THIS
  }
}
```

### 2. Event Listener Memory Leaks

Our current pattern re-creates listeners on every render, which is fine for now but watch for:
- Listeners on `window` or `document` (persist across renders)
- Timers or intervals not cleaned up
- Long-lived references in closures

## Questions to Ask When Adding Features

1. **State**: Does this need to be in `AppState`? Will other components need it?
2. **Rendering**: Is this a pure function? Can it be tested independently?
3. **Events**: Should this use delegation or direct listener?
4. **Types**: What TypeScript types/interfaces are needed?
5. **Theme**: Does this work in both light and dark mode?
6. **Performance**: Will this scale to 100+ terminals?
