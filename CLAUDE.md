# Loom - AI Development Context

## Project Overview

**Loom** is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. Think of it as a visual terminal manager where each terminal can be assigned to an AI agent working on different features simultaneously.

### Core Concept

- **Primary Terminal**: Large display showing the currently selected terminal
- **Mini Terminal Row**: Horizontal strip at bottom showing all active terminals
- **AI Orchestration**: Each terminal can run an AI agent working on different git worktrees
- **GitHub Coordination**: Agents create PRs, issues serve as task queue

### Current Status

- ✅ Issue #1: Basic Tauri setup with TypeScript, TailwindCSS, dark/light theme
- ✅ Issue #2: Layout structure with terminal management (in progress)
- ⏳ Issue #3: Daemon architecture (planned)
- ⏳ Issue #4: Terminal display integration (planned)
- ⏳ Issue #5: AI agent integration (planned)

## Technology Stack

### Frontend
- **Tauri 1.8.1**: Desktop app framework (Rust backend, web frontend)
- **TypeScript 5.9**: Strict mode enabled for maximum type safety
- **Vite 5**: Fast build tool with hot module replacement
- **TailwindCSS 3.4**: Utility-first CSS with dark mode support
- **Vanilla TS**: No framework overhead, direct DOM manipulation

### Backend
- **Rust**: Tauri backend (minimal surface area currently)
- **Node.js**: For terminal process management (future)
- **Anthropic Claude**: AI agent integration (future)

### Why Vanilla TypeScript?

We deliberately chose vanilla TS over React/Vue/Svelte for:
1. **Performance**: Direct DOM manipulation, no virtual DOM overhead
2. **Learning**: Perfect for understanding fundamentals
3. **Simplicity**: No build complexity, no framework lock-in
4. **Control**: Full control over rendering and updates

## Project Structure

```
loom/
├── src/
│   ├── main.ts              # Entry point, state initialization, event handlers
│   ├── style.css            # Global styles, Tailwind imports, transitions
│   └── lib/
│       ├── state.ts         # State management (observer pattern)
│       ├── ui.ts            # UI rendering (pure functions)
│       └── theme.ts         # Dark/light theme system
├── src-tauri/
│   ├── src/main.rs          # Rust backend, Tauri commands
│   ├── tauri.conf.json      # Window config, build settings
│   └── Cargo.toml           # Rust dependencies
├── index.html               # HTML structure (3-section layout)
├── tsconfig.json            # TypeScript strict mode config
├── tailwind.config.js       # Tailwind with dark mode: 'class'
├── vite.config.ts           # Vite config for Tauri
└── package.json             # Dependencies, scripts (uses pnpm)
```

## Architecture Patterns

### 1. Observer Pattern (State Management)

**File**: `src/lib/state.ts`

```typescript
export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private listeners: Set<() => void> = new Set();

  // Notify all listeners when state changes
  private notify(): void {
    this.listeners.forEach(cb => cb());
  }

  // Subscribe to state changes
  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }
}
```

**Why Observer Pattern?**
- Decouples state from UI
- Single source of truth
- Automatic UI updates on state changes
- Easy to add new listeners (e.g., persist to localStorage)

**Key Features**:
- Map-based storage for O(1) terminal lookups
- Strong typing with `Terminal` interface and `TerminalStatus` enum
- Safety: Cannot remove last terminal
- Auto-promotion: First terminal becomes primary when current removed

### 2. Pure Functions (UI Rendering)

**File**: `src/lib/ui.ts`

All rendering functions are pure - same input always produces same output:

```typescript
export function renderPrimaryTerminal(terminal: Terminal | null): void {
  const container = document.getElementById('primary-terminal');
  if (!container) return;

  // Pure transformation: terminal data → HTML string
  container.innerHTML = createPrimaryTerminalHTML(terminal);
}
```

**Why Pure Functions?**
- Predictable and testable
- No hidden side effects
- Easy to reason about
- Can be memoized later for performance

**XSS Protection**: All user input goes through `escapeHtml()` before rendering

### 3. Event Delegation

**File**: `src/main.ts`

Instead of adding listeners to each terminal card, we use delegation:

```typescript
// One listener on parent handles all mini terminal clicks
document.getElementById('mini-terminal-row')?.addEventListener('click', (e) => {
  const target = e.target as HTMLElement;
  const card = target.closest('[data-terminal-id]');

  if (card && !target.classList.contains('close-terminal-btn')) {
    const id = card.getAttribute('data-terminal-id');
    if (id) state.setPrimary(id);
  }
});
```

**Why Event Delegation?**
- Better performance (fewer listeners)
- Works with dynamically added elements
- Simpler cleanup (no need to remove individual listeners)

### 4. Reactive Rendering

The render cycle:

```
State Change → notify() → onChange callbacks → render() → setupEventListeners()
```

**Important**: `setupEventListeners()` is called after every render to re-attach handlers to new DOM elements. This is intentional and works because:
1. Old elements are removed (garbage collected)
2. New elements need fresh event listeners
3. Event delegation minimizes performance impact

## TypeScript Conventions

### Strict Mode

`tsconfig.json` has strict mode enabled:
- `strict: true` - All strict checks
- `noUnusedLocals: true` - No unused variables
- `noUnusedParameters: true` - No unused function parameters
- `noFallthroughCasesInSwitch: true` - Explicit breaks in switch

### Type Safety Patterns

1. **Enums for fixed sets**:
   ```typescript
   export enum TerminalStatus {
     Idle = 'idle',
     Busy = 'busy',
     NeedsInput = 'needs_input',
     Error = 'error',
     Stopped = 'stopped'
   }
   ```

2. **Interfaces for data structures**:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
   }
   ```

3. **Return types for cleanup**:
   ```typescript
   onChange(callback: () => void): () => void {
     this.listeners.add(callback);
     return () => this.listeners.delete(callback); // Cleanup function
   }
   ```

## Styling Conventions

### TailwindCSS Usage

1. **Utility-first**: Use Tailwind classes directly in HTML/JS
2. **Dark mode**: All colors have `dark:` variants
3. **Transitions**: Global 300ms transitions in `style.css`
4. **Semantic colors**: Status indicators use semantic mapping

```typescript
function getStatusColor(status: TerminalStatus): string {
  return {
    [TerminalStatus.Idle]: 'bg-green-500',
    [TerminalStatus.Busy]: 'bg-blue-500',
    [TerminalStatus.NeedsInput]: 'bg-yellow-500',
    [TerminalStatus.Error]: 'bg-red-500',
    [TerminalStatus.Stopped]: 'bg-gray-400'
  }[status];
}
```

### Theme System

**File**: `src/lib/theme.ts`

- Dark mode via `class="dark"` on `<html>`
- Persists to localStorage
- Respects system preference on first load
- 300ms smooth transitions for all color changes

```typescript
export function toggleTheme(): void {
  const isDark = document.documentElement.classList.toggle('dark');
  localStorage.setItem('theme', isDark ? 'dark' : 'light');
}
```

### Custom CSS

Minimal custom CSS in `src/style.css`:
- Tailwind imports
- Global transitions
- Custom scrollbars (webkit)
- Smooth scrolling for mini terminal row

## State Flow Diagram

```
┌─────────────┐
│   AppState  │  (Single source of truth)
└──────┬──────┘
       │
       │ addTerminal()
       │ removeTerminal()
       │ setPrimary()
       │
       ↓
   ┌───────┐
   │notify()│
   └───┬───┘
       │
       ↓
┌──────────────┐
│  onChange    │  (Multiple listeners)
│  callbacks   │
└──────┬───────┘
       │
       ↓
   ┌────────┐
   │render()│  (Re-render entire UI)
   └────┬───┘
        │
        ├──→ renderHeader()
        ├──→ renderPrimaryTerminal()
        └──→ renderMiniTerminals()
             └──→ setupEventListeners()
```

## Common Tasks

### Adding a New Terminal Property

1. Update interface in `src/lib/state.ts`:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
     workingDirectory?: string; // NEW
   }
   ```

2. Update UI rendering in `src/lib/ui.ts`:
   ```typescript
   // Display new property
   <span>${escapeHtml(terminal.workingDirectory || 'N/A')}</span>
   ```

3. TypeScript will catch any missing properties at compile time

### Adding a New State Method

1. Add method to `AppState` class in `src/lib/state.ts`
2. Call `this.notify()` after state changes
3. UI will automatically re-render

### Adding a New UI Section

1. Add HTML structure to `index.html`
2. Create render function in `src/lib/ui.ts`
3. Call from `render()` in `src/main.ts`
4. Add event listeners in `setupEventListeners()`

### Debugging

1. **State inspection**: Add `console.log(state.getTerminals())` in render
2. **TypeScript errors**: Run `pnpm exec tsc --noEmit`
3. **Hot reload**: Vite provides instant feedback on save
4. **Tauri DevTools**: Open with Cmd+Option+I in dev mode

## Testing Strategy

### Current State (Manual Testing)

- Launch app: `pnpm tauri:dev`
- Manual interaction testing
- TypeScript strict mode catches type errors
- No runtime errors in console

### Future Testing (Planned)

1. **Unit Tests**: Vitest for pure functions (state.ts, ui.ts)
2. **Integration Tests**: Playwright for E2E workflows
3. **Type Tests**: TypeScript as first line of defense

## Performance Considerations

### Current Optimizations

1. **Map-based state**: O(1) terminal lookups
2. **Event delegation**: Minimal listener count
3. **Pure functions**: Easy to optimize later with memoization
4. **No virtual DOM**: Direct DOM manipulation

### Future Optimizations

1. **Virtual scrolling**: For 100+ terminals in mini row
2. **Memoization**: Cache rendered HTML for unchanged terminals
3. **Web Workers**: Move state logic off main thread
4. **Incremental rendering**: Only update changed sections

## Security Considerations

### XSS Prevention

All user input is escaped before rendering:

```typescript
function escapeHtml(text: string): string {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
```

This prevents malicious terminal names from injecting HTML/JS.

### Future Security

- Tauri IPC will be used for process spawning (sandboxed)
- API keys stored in system keychain (not .env)
- GitHub OAuth for authentication

## Git Workflow

### Branch Strategy

- `main`: Always stable, ready to release
- `feature/issue-X-description`: Feature branches from issues
- PR required for merge to main

### Commit Convention

```
<type>: <short description>

<longer description>

<footer>
```

Example:
```
Implement initial layout structure with terminal management

Build core UI layout with header, primary terminal view, mini terminal row...

Closes #2
```

### PR Process

1. Create feature branch from main
2. Implement feature
3. Test manually (`pnpm tauri:dev`)
4. Verify TypeScript (`pnpm exec tsc --noEmit`)
5. Create PR with detailed description
6. Merge after review

## Future Architecture (Issues #3-5)

### Issue #3: Daemon Architecture

**Goal**: Background process managing all terminals

```
Tauri App (UI) ←─ IPC ─→ Daemon (Node.js) ←─→ Terminal Processes
```

### Issue #4: Terminal Display

**Goal**: Real terminal emulator in primary view

Technology candidates:
- xterm.js (battle-tested)
- zutty (modern, GPU-accelerated)
- Custom implementation

### Issue #5: AI Agent Integration

**Goal**: Claude agents working in terminals

```
Daemon → Spawn terminal with Claude
       → Claude reads/writes terminal
       → Creates git commits/PRs
       → Updates issue status
```

## Key Design Decisions

### Why Tauri over Electron?

1. **Performance**: Rust backend, native webview
2. **Security**: Smaller attack surface
3. **Size**: ~10MB vs ~100MB for Electron apps
4. **Modern**: Built for modern web standards

### Why Map over Array for State?

1. **Performance**: O(1) lookups by ID
2. **Semantics**: Terminals have unique IDs
3. **Flexibility**: Easy to add indexed access later

### Why No React/Vue?

1. **Simplicity**: This is a learning project
2. **Performance**: Direct DOM manipulation is fast
3. **Size**: No framework overhead
4. **Control**: Full control over rendering

### Why Class for State?

1. **Encapsulation**: Private fields and methods
2. **TypeScript**: Full type checking
3. **Familiarity**: OOP pattern many devs know
4. **Extensibility**: Easy to add methods

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

### 3. Missing Dark Mode Variants

Every color class needs a `dark:` variant:

```html
<!-- WRONG -->
<div class="bg-gray-100 text-gray-900">

<!-- RIGHT -->
<div class="bg-gray-100 dark:bg-gray-800 text-gray-900 dark:text-gray-100">
```

### 4. Inline Styles vs Tailwind

Prefer Tailwind classes over inline styles for theme support:

```html
<!-- WRONG (doesn't respect theme) -->
<div style="background-color: #1a1a1a">

<!-- RIGHT (respects theme) -->
<div class="bg-gray-900 dark:bg-gray-800">
```

## Questions to Ask When Adding Features

1. **State**: Does this need to be in `AppState`? Will other components need it?
2. **Rendering**: Is this a pure function? Can it be tested independently?
3. **Events**: Should this use delegation or direct listener?
4. **Types**: What TypeScript types/interfaces are needed?
5. **Theme**: Does this work in both light and dark mode?
6. **Performance**: Will this scale to 100+ terminals?

## Resources

- **Tauri Docs**: https://tauri.app/v1/guides/
- **TypeScript Handbook**: https://www.typescriptlang.org/docs/
- **TailwindCSS Docs**: https://tailwindcss.com/docs
- **GitHub Issues**: Track work and discuss architecture
- **CLAUDE.md**: You're reading it! Keep this updated.

## Maintaining This Document

This document should evolve as the project grows:

1. **When adding patterns**: Document the pattern and rationale
2. **When making architectural decisions**: Add to "Key Design Decisions"
3. **When finding pitfalls**: Add to "Common Pitfalls"
4. **When removing code**: Update relevant sections

Keep this as a living document that helps both humans and AI understand the codebase deeply.

---

Last updated: Issue #2 (Layout Structure) - Terminal management UI complete
