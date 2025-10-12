# Loom - AI Development Context

## Project Overview

**Loom** is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. Think of it as a visual terminal manager where each terminal can be assigned to an AI agent working on different features simultaneously.

### Core Concept

- **Primary Display**: Large view showing the currently selected agent terminal
- **Mini Terminal Row**: Horizontal strip at bottom showing all active agent terminals
- **Workspace Selection**: Git repository workspace picker with validation
- **AI Orchestration**: Each agent terminal works on different features in git worktrees
- **GitHub Coordination**: Agents create PRs, issues serve as task queue

### Current Status

- ✅ Issue #1: Basic Tauri setup with TypeScript, TailwindCSS, dark/light theme
- ✅ Issue #2: Layout structure with agent management and workspace selection
- ✅ Issue #3: Daemon architecture with Rust and tmux
- ✅ Issue #4: Terminal display with xterm.js
- ✅ Issue #5: Worker launcher with Claude Code
- ✅ Issue #8: Comprehensive linting, formatting, and CI/CD setup
- ✅ Issue #13: Daemon integration tests with full IPC coverage
- ⏳ Issue #6: .loom/ directory configuration (planned)
- ⏳ Issue #7: Workspace selector improvements (planned)

### Recent Features (Issue #2)

- **Agent Management**: Create, close, rename, and reorder agent terminals
- **Workspace Selection**: Native folder picker with git repository validation
- **Persistent Config**: Agent counter stored in `.loom/config.json` per workspace
- **Monotonic Numbering**: Agents always increment, persists across app restarts
- **Drag & Drop**: Reorder agent terminals in the mini row
- **Inline Renaming**: Double-click agent names to rename (doesn't affect numbering)
- **Tilde Expansion**: Support for `~/path` notation in workspace paths
- **Workspace-First**: Must select workspace before creating agents

## Technology Stack

### Frontend
- **Tauri 1.8.1**: Desktop app framework (Rust backend, web frontend)
- **TypeScript 5.9**: Strict mode enabled for maximum type safety
- **Vite 5**: Fast build tool with hot module replacement
- **TailwindCSS 3.4**: Utility-first CSS with dark mode support
- **Vanilla TS**: No framework overhead, direct DOM manipulation

### Backend
- **Rust**: Tauri backend with IPC commands
  - `validate_git_repo`: Validates git repository paths
  - `greet`: Example command (will be removed)
- **Tauri APIs**: Dialog (file picker), Path (tilde expansion)
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
│   ├── main.ts              # Entry point, state init, events, workspace logic
│   ├── style.css            # Global styles, Tailwind imports
│   └── lib/
│       ├── state.ts         # State management (agents, workspace, observer)
│       ├── config.ts        # Config file I/O (.loom/config.json)
│       ├── ui.ts            # UI rendering (pure functions)
│       └── theme.ts         # Dark/light theme system
├── src-tauri/
│   ├── src/main.rs          # Rust backend, Tauri IPC commands
│   ├── tauri.conf.json      # Window config, allowlist, build settings
│   └── Cargo.toml           # Rust dependencies (tauri features)
├── .loom/                   # Workspace config (gitignored, per-workspace)
│   └── config.json          # Persistent config (agent counter, etc.)
├── index.html               # HTML structure (header, primary, mini row)
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
- Map-based storage for O(1) agent terminal lookups
- Strong typing with `Terminal` interface and `TerminalStatus` enum
- Safety: Cannot remove last agent terminal
- Auto-promotion: First terminal becomes primary when current removed
- Workspace state: Separate valid workspace vs displayed path for error handling
- Monotonic agent numbering: Counter always increments, never reuses deleted numbers

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

### 5. Tauri IPC (Inter-Process Communication)

**Files**: `src/main.ts`, `src-tauri/src/main.rs`

Tauri provides a bridge between TypeScript frontend and Rust backend:

**Frontend** (TypeScript):
```typescript
import { invoke } from '@tauri-apps/api/tauri';

const isValid = await invoke<boolean>('validate_git_repo', { path });
```

**Backend** (Rust):
```rust
#[tauri::command]
fn validate_git_repo(path: String) -> Result<bool, String> {
    // Validation logic with full filesystem access
}
```

**Why Use Rust Commands?**
- Bypass client-side filesystem restrictions
- Full native filesystem access
- Type-safe IPC with automatic serialization
- Better error handling and security

**Current Commands**:
- `validate_git_repo(path: String)`: Validates path is a git repository
  - Checks path exists and is a directory
  - Verifies `.git` directory exists
  - Returns `Result<bool, String>` with specific error messages

**Workspace Validation Pattern**:
```typescript
// Separate state: displayedWorkspacePath (shown) vs workspacePath (valid)
state.setDisplayedWorkspace(userInput);  // Show immediately
const isValid = await validateWorkspacePath(userInput);
if (isValid) {
  state.setWorkspace(userInput);  // Mark as valid
} else {
  state.setWorkspace('');  // Keep displayed but don't use
}
```

This allows showing invalid paths with error messages while preventing use of invalid workspace.

### 6. Persistent Configuration

**Files**: `src/lib/config.ts`, `.loom/config.json`

Loom stores workspace-specific configuration in `.loom/config.json` within each git repository:

```json
{
  "nextAgentNumber": 6
}
```

**Why Workspace-Specific Config?**
- Each git repo has independent agent numbering
- Config persists across app restarts
- No parsing of agent names (users can rename freely)
- Stored in workspace, not in app directory

**Config Lifecycle**:
```typescript
// 1. User selects workspace
await handleWorkspacePathInput('/path/to/repo');

// 2. Set config workspace path
setConfigWorkspace('/path/to/repo');

// 3. Load config from .loom/config.json
const config = await loadConfig();  // { nextAgentNumber: 1 } or existing

// 4. Initialize state counter
state.setNextAgentNumber(config.nextAgentNumber);

// 5. User creates agent
const num = state.getNextAgentNumber();  // Returns 1, increments to 2
state.addTerminal({ name: `Agent ${num}`, ... });

// 6. Save updated counter
await saveConfig({ nextAgentNumber: state.getCurrentAgentNumber() });
```

**File Operations**:
- Uses Tauri fs API (`readTextFile`, `writeTextFile`, `exists`, `createDir`)
- Creates `.loom/` directory if it doesn't exist
- Falls back to defaults if config file missing
- Gracefully handles read/write errors

**Important**: `.loom/` is gitignored - each developer has their own agent numbering.

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

## Code Quality Tools (Issue #8)

### Linting & Formatting Setup

**Frontend (Biome)**:
- Fast, comprehensive linter and formatter for TypeScript/JavaScript
- Configured in `biome.json` with schema version 2.2.5
- VCS integration enabled for git-aware linting
- Rules: Recommended + custom overrides for project style
- Commands: `npm run lint`, `npm run format`

**Backend (rustfmt + clippy)**:
- `rustfmt.toml`: Format configuration (100 char width, 4 space indent)
- `.cargo/config.toml`: Clippy lint levels
  - Deny: all, correctness, suspicious, complexity
  - Warn: pedantic, perf, style, unwrap_used, expect_used
- Commands: `npm run format:rust`, `npm run clippy`

**Git Hooks (husky + lint-staged)**:
- Pre-commit hook auto-formats staged files
- TS/JS files: Biome formatting + linting
- Rust files: rustfmt formatting
- Configured in `.husky/pre-commit` and `package.json`

**CI/CD (GitHub Actions)**:
- Workflow: `.github/workflows/ci.yml`
- Jobs run in parallel: frontend lint/format, rust format, rust clippy, builds
- All warnings treated as errors (`-D warnings` for clippy)
- Dependency caching for faster builds
- Frontend build artifacts downloaded before Tauri compilation

**VSCode Integration**:
- Settings: `.vscode/settings.json`
- Extensions: `.vscode/extensions.json`
- Format on save enabled for all languages
- Biome for TS/JS, rust-analyzer for Rust

### Development Workflow

1. **Make changes** - Edit code with format-on-save
2. **Pre-commit hook** - Auto-formats on commit
3. **Push** - Triggers CI checks
4. **CI validates** - All linting/formatting/builds must pass
5. **Manual check** - Run `npm run check:all` to verify locally

### Clippy Configuration Details

The `.cargo/config.toml` enforces strict linting:

```toml
rustflags = [
    "-D", "clippy::all",           # Deny all warnings
    "-D", "clippy::correctness",   # Deny correctness issues
    "-D", "clippy::suspicious",    # Deny suspicious patterns
    "-D", "clippy::complexity",    # Deny unnecessary complexity
    "-W", "clippy::pedantic",      # Warn on pedantic issues
    "-W", "clippy::unwrap_used",   # Warn on .unwrap()
    "-W", "clippy::expect_used",   # Warn on .expect()
]
```

**When to use `#[allow(clippy::expect_used)]`**:
- Mutex locks (poisoning is panic-level, not recoverable)
- Main function startup (Tauri failure is fatal)
- Other truly exceptional scenarios

**Handling expect/unwrap warnings**:
- Prefer proper error handling with `Result` and `?` operator
- Use `expect()` with descriptive messages only when panic is acceptable
- Add `#[allow]` attribute with explanatory comment when necessary

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
- Instant color changes (no transitions for better UX)

```typescript
export function toggleTheme(): void {
  const isDark = document.documentElement.classList.toggle('dark');
  localStorage.setItem('theme', isDark ? 'dark' : 'light');
}
```

**Design Choice**: Theme transitions were intentionally removed because animated color changes during theme toggle were distracting and made the interface feel sluggish.

### Custom CSS

Minimal custom CSS in `src/style.css`:
- Tailwind imports
- Custom scrollbars (webkit)
- Smooth scrolling for mini terminal row
- Drop indicator for drag-and-drop
- User-select: none on draggable cards

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

### Adding a New Agent Terminal Property

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

### Adding a New Tauri Command

1. **Add Rust command** in `src-tauri/src/main.rs`:
   ```rust
   #[tauri::command]
   fn my_command(param: String) -> Result<ReturnType, String> {
       // Implementation
       Ok(result)
   }
   ```

2. **Register command** in `main()`:
   ```rust
   tauri::Builder::default()
       .invoke_handler(tauri::generate_handler![my_command])
   ```

3. **Call from TypeScript** in `src/main.ts`:
   ```typescript
   import { invoke } from '@tauri-apps/api/tauri';

   const result = await invoke<ReturnType>('my_command', { param: value });
   ```

4. **Add required APIs** to `src-tauri/tauri.conf.json` allowlist if needed
5. **Update Cargo.toml** if new Tauri features required

### Debugging

1. **State inspection**: Add `console.log(state.getTerminals())` in render
2. **TypeScript errors**: Run `pnpm exec tsc --noEmit`
3. **Hot reload**: Vite provides instant feedback on save
4. **Tauri DevTools**: Open with Cmd+Option+I in dev mode

## Testing Strategy

### Daemon Integration Tests (Issue #13)

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

### Frontend Testing (Planned)

1. **Unit Tests**: Vitest for pure functions (state.ts, ui.ts)
2. **Integration Tests**: Playwright for E2E workflows
3. **Type Tests**: TypeScript strict mode as first line of defense

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

### Why HTML5 Drag API (Not Mouse Events)?

We use HTML5 drag events (`dragstart`, `dragover`, `dragend`) despite initial complexity:

1. **Native behavior**: Browser handles drag cursor and visual feedback
2. **Accessibility**: Screen readers understand drag operations
3. **Cross-platform**: Works consistently across operating systems
4. **No conflicts**: Doesn't interfere with text selection when properly configured

**Key Implementation Details**:
- Use `dragend` (not `drop`) because Tauri webview doesn't always fire `drop` events
- Set `user-select: none` on cards to prevent text selection during drag
- Use border-based selection (not `ring`/`outline`) to prevent clipping during drag
- Wrapper divs with padding ensure borders don't get cut off
- Drop indicator shows insertion point using absolute positioning

### Why Separate Workspace State Variables?

We maintain `workspacePath` (valid) and `displayedWorkspacePath` (shown):

1. **Better UX**: Don't clear user input when path is invalid
2. **Error display**: Show specific validation errors while keeping input
3. **State clarity**: Distinguish between "what user typed" and "what we'll use"
4. **Validation workflow**: User sees path → validate → either accept or show error

This pattern prevents frustrating behavior where invalid input disappears.

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

Last updated: Issue #13 (Daemon Integration Tests) - Complete test infrastructure with 9 passing integration tests covering IPC, terminal lifecycle, and error handling
