# ADR-0002: Vanilla TypeScript over React/Vue/Svelte

## Status

Accepted

## Context

Loom needed a frontend technology choice for building the desktop UI. The application has relatively simple UI requirements:
- Header with workspace selector and theme toggle
- Primary terminal view (large display area)
- Mini terminal row (horizontal strip of terminal cards)
- Drag-and-drop reordering
- Terminal settings modal

Modern frameworks (React, Vue, Svelte) were considered alongside vanilla TypeScript with direct DOM manipulation.

## Decision

Build the frontend with **Vanilla TypeScript** using direct DOM manipulation, avoiding frameworks entirely.

Technical approach:
- TypeScript strict mode for type safety
- Pure rendering functions (same input → same output)
- Event delegation for performance
- Observer pattern for state management (see ADR-0001)
- Tailwind CSS for styling

## Consequences

### Positive

- **Performance**: Direct DOM manipulation, no virtual DOM overhead
- **Learning value**: Forces understanding of fundamental web APIs
- **Simplicity**: No build complexity, no framework lock-in
- **Full control**: Complete control over rendering and update cycles
- **Small bundle**: No framework code to ship (~10MB app vs ~100MB with Electron + React)
- **Fast iteration**: Changes are immediately visible with hot reload
- **No framework churn**: Won't be affected by framework version updates

### Negative

- **More boilerplate**: Manually creating HTML strings and event listeners
- **No component ecosystem**: Can't use React/Vue component libraries
- **Manual optimization**: Must implement own memoization if needed
- **Event listener management**: Manual cleanup required
- **Less familiar**: Many developers expect React/Vue patterns
- **Type safety gaps**: No JSX type checking for HTML strings

## Alternatives Considered

### 1. React

**Pros**:
- Huge ecosystem of components
- Excellent DevTools
- Well-known patterns
- JSX type safety

**Rejected because**:
- Virtual DOM overhead for simple UI
- Build complexity (Babel, webpack config)
- Large bundle size
- Framework lock-in
- Overkill for terminal display app

### 2. Vue

**Pros**:
- Gentle learning curve
- Excellent documentation
- Template syntax familiar to HTML
- Good TypeScript support

**Rejected because**:
- Still a framework dependency
- Virtual DOM overhead
- Not as instructive for learning
- Bundle size concerns

### 3. Svelte

**Pros**:
- Compiles to vanilla JS (no runtime)
- Excellent performance
- Small bundle size
- Simple reactive syntax

**Rejected because**:
- Compile step adds complexity
- Newer framework (less mature ecosystem)
- Less familiar to contributors
- Wanted pure TypeScript learning experience

### 4. Lit (Web Components)

**Pros**:
- Standards-based (Web Components)
- Small runtime
- Good TypeScript support

**Rejected because**:
- Still a library dependency
- Web Components have browser quirks
- More complex than vanilla TS

## References

- Implementation: `src/lib/ui.ts` (pure rendering functions)
- Related: ADR-0001 (Observer Pattern)
- Related: CLAUDE.md "Why Vanilla TypeScript?" section
- Tauri documentation: https://tauri.app/v1/guides/
