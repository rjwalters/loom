# ADR-0005: HTML5 Drag API over Mouse Events

## Status

Accepted

## Context

Loom's mini terminal row allows users to reorder agent terminals via drag-and-drop. Two approaches were considered:

1. **HTML5 Drag and Drop API**: Native browser drag events (`dragstart`, `dragover`, `drop`, `dragend`)
2. **Mouse Events**: Custom implementation using `mousedown`, `mousemove`, `mouseup`

The HTML5 API has a reputation for complexity, but provides native behavior. Mouse events offer more control but require handling all edge cases manually.

## Decision

Use the **HTML5 Drag and Drop API** with these specific implementation choices:

- `dragstart`: Set `dataTransfer` with terminal ID
- `dragover`: Prevent default and show drop indicator
- `dragend`: Handle drop (not `drop` event - Tauri webview bug)
- `user-select: none`: Prevent text selection during drag
- Border-based selection: Avoid `ring`/`outline` clipping issues
- Wrapper divs with padding: Prevent border cutoff during drag

## Consequences

### Positive

- **Native behavior**: Browser handles drag cursor, ghost image, visual feedback
- **Accessibility**: Screen readers understand native drag operations
- **Cross-platform**: Consistent behavior across macOS, Windows, Linux
- **No conflicts**: Doesn't interfere with text selection when properly configured
- **Less code**: Browser handles drag state, cursor changes, drop validation
- **Standard API**: Well-documented, widely understood pattern

### Negative

- **Datasheet complexity**: HTML5 Drag API has quirks (e.g., `effectAllowed`)
- **Tauri bug workaround**: Must use `dragend` instead of `drop` event
- **CSS conflicts**: Requires `user-select: none` to prevent text selection
- **Clipping issues**: Must use borders (not outlines) to prevent cutoff
- **Browser inconsistencies**: Some edge cases behave differently across browsers

## Alternatives Considered

### 1. Mouse Events Implementation

**Pros**:
- Full control over drag behavior
- No browser quirks
- Simpler mental model (just track mouse position)

**Rejected because**:
- Must manually handle cursor changes
- Must track drag state explicitly
- Must handle edge cases (drag outside window, release during drag)
- Must implement own drop validation
- No accessibility support
- More code to maintain

### 2. Library (react-beautiful-dnd, SortableJS)

**Pros**:
- Battle-tested implementations
- Handle edge cases automatically
- Good accessibility support

**Rejected because**:
- External dependencies (against vanilla TS principle)
- Overkill for simple reordering
- Bundle size increase
- Learning curve for library API
- Related: ADR-0002 (Vanilla TypeScript)

### 3. Hybrid Approach (Mouse events with drag cursor CSS)

**Pros**:
- Custom behavior with some native UX
- More control than pure HTML5

**Rejected because**:
- Still requires handling all mouse event edge cases
- Doesn't solve accessibility issues
- More complex than either pure approach

## Implementation Details

**Drag Handlers** (`src/main.ts`):
```typescript
card.addEventListener('dragstart', (e: DragEvent) => {
  e.dataTransfer?.setData('text/plain', terminalId);
  e.dataTransfer.effectAllowed = 'move';
});

card.addEventListener('dragend', (e: DragEvent) => {
  // Handle drop here (Tauri webview doesn't fire 'drop' reliably)
  const targetId = getClosestTerminalId(e.clientX);
  if (targetId) state.reorderTerminal(draggedId, targetId);
});
```

**CSS Requirements** (`src/style.css`):
```css
.terminal-card {
  user-select: none; /* Prevent text selection during drag */
  cursor: grab;
}

.terminal-card:active {
  cursor: grabbing;
}

.selected-terminal {
  border: 2px solid theme('colors.blue.500'); /* Not outline - prevents clipping */
}
```

**Tauri Workaround**:
The Tauri webview doesn't reliably fire `drop` events, so we use `dragend` and manually determine the drop target from cursor position. This is a known Tauri limitation, not an HTML5 Drag API issue.

## References

- Implementation: `src/main.ts` (event listeners), `src/style.css` (drag styles)
- Related: ADR-0002 (Vanilla TypeScript)
- MDN Drag API: https://developer.mozilla.org/en-US/docs/Web/API/HTML_Drag_and_Drop_API
- Tauri Issue: https://github.com/tauri-apps/tauri/issues/XXXX
