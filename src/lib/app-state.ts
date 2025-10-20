/**
 * App-level UI state management
 *
 * This module manages transient UI state that doesn't belong in the domain
 * model (AppState). It handles:
 * - Currently attached terminal ID (xterm.js instance management)
 * - Drag and drop state for terminal reordering
 *
 * Extracted from main.ts and drag-drop-manager.ts as part of Issue #118 PR 1.
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
 * App-level state manager for UI-specific state
 *
 * This class manages state that is specific to the UI layer and doesn't
 * belong in the domain model. It uses the singleton pattern for global access.
 */
export class AppLevelState {
  private currentAttachedTerminalId: string | null = null;
  private dragState: DragState = {
    draggedConfigId: null,
    dropTargetConfigId: null,
    dropInsertBefore: false,
    isDragging: false,
  };

  /**
   * Gets the ID of the currently attached terminal (xterm.js instance)
   */
  getCurrentAttachedTerminalId(): string | null {
    return this.currentAttachedTerminalId;
  }

  /**
   * Sets the ID of the currently attached terminal
   * @param id - The terminal ID, or null to clear
   */
  setCurrentAttachedTerminalId(id: string | null): void {
    this.currentAttachedTerminalId = id;
  }

  /**
   * Gets the ID of the terminal being dragged
   */
  getDraggedConfigId(): string | null {
    return this.dragState.draggedConfigId;
  }

  /**
   * Sets the ID of the terminal being dragged
   * @param id - The terminal ID, or null to clear
   */
  setDraggedConfigId(id: string | null): void {
    this.dragState.draggedConfigId = id;
  }

  /**
   * Gets the ID of the terminal being dropped onto
   */
  getDropTargetConfigId(): string | null {
    return this.dragState.dropTargetConfigId;
  }

  /**
   * Sets the ID of the terminal being dropped onto
   * @param id - The terminal ID, or null to clear
   */
  setDropTargetConfigId(id: string | null): void {
    this.dragState.dropTargetConfigId = id;
  }

  /**
   * Gets whether to insert before (true) or after (false) the drop target
   */
  getDropInsertBefore(): boolean {
    return this.dragState.dropInsertBefore;
  }

  /**
   * Sets whether to insert before (true) or after (false) the drop target
   * @param insertBefore - True to insert before, false to insert after
   */
  setDropInsertBefore(insertBefore: boolean): void {
    this.dragState.dropInsertBefore = insertBefore;
  }

  /**
   * Gets whether a drag operation is currently in progress
   */
  getIsDragging(): boolean {
    return this.dragState.isDragging;
  }

  /**
   * Sets whether a drag operation is currently in progress
   * @param isDragging - True if dragging, false otherwise
   */
  setIsDragging(isDragging: boolean): void {
    this.dragState.isDragging = isDragging;
  }

  /**
   * Gets all drag state at once
   */
  getDragState(): DragState {
    return { ...this.dragState };
  }

  /**
   * Resets all drag state to initial values
   */
  resetDragState(): void {
    this.dragState = {
      draggedConfigId: null,
      dropTargetConfigId: null,
      dropInsertBefore: false,
      isDragging: false,
    };
  }
}

// Singleton instance for accessing app-level state from anywhere
let appLevelStateInstance: AppLevelState | null = null;

/**
 * Gets the singleton AppLevelState instance.
 * Creates a new instance if one doesn't exist.
 * Use this to access app-level UI state from anywhere in the codebase.
 *
 * @returns The singleton AppLevelState instance
 */
export function getAppLevelState(): AppLevelState {
  if (!appLevelStateInstance) {
    appLevelStateInstance = new AppLevelState();
  }
  return appLevelStateInstance;
}

/**
 * Sets the singleton AppLevelState instance.
 * Primarily used for testing to inject a mock state instance.
 *
 * @param state - The AppLevelState instance to use as the singleton
 */
export function setAppLevelState(state: AppLevelState): void {
  appLevelStateInstance = state;
}
