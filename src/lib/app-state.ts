/**
 * App-level UI state management
 *
 * This module manages transient UI state that doesn't belong in the domain
 * model (AppState). It handles:
 * - Currently attached terminal ID (xterm.js instance management)
 * - Drag and drop state for terminal reordering
 *
 * Simplified from class-based implementation to plain object (Issue #629).
 */

/**
 * Drag and drop state for terminal reordering
 */
export interface DragState {
  /** ID of the terminal being dragged */
  draggedConfigId: string | null;
  /** ID of the terminal being dropped onto */
  dropTargetConfigId: string | null;
  /** Whether to insert before (true) or after (false) the drop target */
  dropInsertBefore: boolean;
  /** Whether a drag operation is currently in progress */
  isDragging: boolean;
}

/**
 * App-level state interface for UI-specific state
 */
export interface AppLevelState {
  /** ID of the currently attached terminal (xterm.js instance) */
  currentAttachedTerminalId: string | null;
  /** Drag and drop state */
  dragState: DragState;
  /** Whether user is actively editing (skip re-renders during edits) */
  isUserEditing: boolean;
}

/**
 * Global app-level state singleton
 *
 * Simple mutable object for managing UI state. Access properties directly.
 * Use resetDragState() helper to reset drag state to initial values.
 */
export const appLevelState: AppLevelState = {
  currentAttachedTerminalId: null,
  dragState: {
    draggedConfigId: null,
    dropTargetConfigId: null,
    dropInsertBefore: false,
    isDragging: false,
  },
  isUserEditing: false,
};

/**
 * Resets all drag state to initial values
 *
 * Helper function for the most common drag state operation.
 */
export function resetDragState(): void {
  appLevelState.dragState = {
    draggedConfigId: null,
    dropTargetConfigId: null,
    dropInsertBefore: false,
    isDragging: false,
  };
}
