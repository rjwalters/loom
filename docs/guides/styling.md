# Styling Conventions

## TailwindCSS Usage

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

## Theme System

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

## Custom CSS

Minimal custom CSS in `src/style.css`:
- Tailwind imports
- Custom scrollbars (webkit)
- Smooth scrolling for mini terminal row
- Drop indicator for drag-and-drop
- User-select: none on draggable cards

## Common Pitfalls

### 1. Missing Dark Mode Variants

Every color class needs a `dark:` variant:

```html
<!-- WRONG -->
<div class="bg-gray-100 text-gray-900">

<!-- RIGHT -->
<div class="bg-gray-100 dark:bg-gray-800 text-gray-900 dark:text-gray-100">
```

### 2. Inline Styles vs Tailwind

Prefer Tailwind classes over inline styles for theme support:

```html
<!-- WRONG (doesn't respect theme) -->
<div style="background-color: #1a1a1a">

<!-- RIGHT (respects theme) -->
<div class="bg-gray-900 dark:bg-gray-800">
```
