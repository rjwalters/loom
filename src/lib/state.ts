import { Logger } from "./logger";
import { TerminalStateManager } from "./terminal-state-manager";
import { WorkspaceStateManager } from "./workspace-state-manager";

// Re-export types and enums for backward compatibility
export type { ActivityEntry, ColorTheme, InputRequest, Terminal } from "./types";
export { AgentStatus, isValidTerminal, TerminalStatus } from "./types";

const logger = Logger.forComponent("state");

/**
 * Central state management for the Loom application.
 * Implements the Observer pattern to automatically update UI when state changes.
 *
 * This class composes focused state managers:
 * - TerminalStateManager: Terminal instances, primary selection, ordering, and configuration
 * - WorkspaceStateManager: Workspace path validation and persistence
 * - AppState: Application-level flags and coordination
 *
 * All state mutations automatically notify registered listeners via the onChange() callback system.
 */
export class AppState {
  // Focused state managers
  readonly terminals: TerminalStateManager;
  readonly workspace: WorkspaceStateManager;

  // Application-level state (not delegated to managers)
  private listeners: Set<() => void> = new Set();
  private isResettingWorkspace: boolean = false; // Loading state during factory reset
  private isInitializing: boolean = false; // Loading state during app startup
  private autoSaveTimer: ReturnType<typeof setTimeout> | null = null; // Timer for debounced auto-save
  private autoSaveCallback: (() => Promise<void>) | null = null; // Callback for auto-saving state
  private offlineMode: boolean = false; // Offline mode flag - when true, skips AI agent launches

  constructor() {
    this.terminals = new TerminalStateManager();
    this.workspace = new WorkspaceStateManager();

    // Propagate changes from managers to global listeners
    this.terminals.onChange(() => this.notify());
    this.workspace.onChange(() => this.notify());
  }

  // ============================================================================
  // Backward Compatibility Layer - Delegates to managers
  // ============================================================================

  /** @deprecated Use state.terminals.addTerminal() instead */
  addTerminal(terminal: import("./types").Terminal): void {
    this.terminals.addTerminal(terminal);
  }

  /** @deprecated Use state.terminals.removeTerminal() instead */
  removeTerminal(id: string): void {
    this.terminals.removeTerminal(id);
  }

  /** @deprecated Use state.terminals.setPrimary() instead */
  setPrimary(id: string): void {
    this.terminals.setPrimary(id);
  }

  /** @deprecated Use state.terminals.renameTerminal() instead */
  renameTerminal(id: string, newName: string): void {
    this.terminals.renameTerminal(id, newName);
  }

  /** @deprecated Use state.terminals.updateTerminal() instead */
  updateTerminal(id: string, updates: Partial<import("./types").Terminal>): void {
    this.terminals.updateTerminal(id, updates);
  }

  /** @deprecated Use state.terminals.updateTerminalStatus() instead */
  updateTerminalStatus(id: string, newStatus: import("./types").TerminalStatus): void {
    this.terminals.updateTerminalStatus(id, newStatus);
  }

  /** @deprecated Use state.terminals.setTerminalRole() instead */
  setTerminalRole(
    id: string,
    role: string | undefined,
    roleConfig?: Record<string, unknown>
  ): void {
    this.terminals.setTerminalRole(id, role, roleConfig);
  }

  /** @deprecated Use state.terminals.setTerminalTheme() instead */
  setTerminalTheme(id: string, themeId: string): void {
    this.terminals.setTerminalTheme(id, themeId);
  }

  /** @deprecated Use state.terminals.setTerminalCustomTheme() instead */
  setTerminalCustomTheme(id: string, theme: import("./types").ColorTheme): void {
    this.terminals.setTerminalCustomTheme(id, theme);
  }

  /** @deprecated Use state.terminals.updateTerminalWorkerType() instead */
  updateTerminalWorkerType(
    id: string,
    workerType: "claude" | "codex" | "github-copilot" | "gemini" | "deepseek" | "grok"
  ): void {
    this.terminals.updateTerminalWorkerType(id, workerType);
  }

  /** @deprecated Use state.terminals.getPrimary() instead */
  getPrimary(): import("./types").Terminal | null {
    return this.terminals.getPrimary();
  }

  /** @deprecated Use state.terminals.hasPrimary() instead */
  hasPrimary(): boolean {
    return this.terminals.hasPrimary();
  }

  /** @deprecated Use state.terminals.getPrimaryOrThrow() instead */
  getPrimaryOrThrow(): import("./types").Terminal {
    return this.terminals.getPrimaryOrThrow();
  }

  /** @deprecated Use state.terminals.getTerminals() instead */
  getTerminals(): import("./types").Terminal[] {
    return this.terminals.getTerminals();
  }

  /** @deprecated Use state.terminals.reorderTerminal() instead */
  reorderTerminal(draggedId: string, targetId: string, insertBefore: boolean): void {
    this.terminals.reorderTerminal(draggedId, targetId, insertBefore);
  }

  /** @deprecated Use state.terminals.getTerminal() instead */
  getTerminal(id: string): import("./types").Terminal | null {
    return this.terminals.getTerminal(id);
  }

  /** @deprecated Use state.terminals.loadTerminals() instead */
  loadAgents(agents: import("./types").Terminal[]): void {
    this.terminals.loadTerminals(agents);
  }

  /** @deprecated Use state.terminals.getNextTerminalNumber() instead */
  getNextTerminalNumber(): number {
    return this.terminals.getNextTerminalNumber();
  }

  /** @deprecated Use state.terminals.setNextTerminalNumber() instead */
  setNextTerminalNumber(num: number): void {
    this.terminals.setNextTerminalNumber(num);
  }

  /** @deprecated Use state.terminals.getCurrentTerminalNumber() instead */
  getCurrentTerminalNumber(): number {
    return this.terminals.getCurrentTerminalNumber();
  }

  /** @deprecated Use state.workspace.setWorkspace() instead */
  setWorkspace(path: string): void {
    this.workspace.setWorkspace(path);
  }

  /** @deprecated Use state.workspace.setDisplayedWorkspace() instead */
  setDisplayedWorkspace(path: string): void {
    this.workspace.setDisplayedWorkspace(path);
  }

  /** @deprecated Use state.workspace.getWorkspace() instead */
  getWorkspace(): string | null {
    return this.workspace.getWorkspace();
  }

  /** @deprecated Use state.workspace.hasWorkspace() instead */
  hasWorkspace(): boolean {
    return this.workspace.hasWorkspace();
  }

  /** @deprecated Use state.workspace.getWorkspaceOrThrow() instead */
  getWorkspaceOrThrow(): string {
    return this.workspace.getWorkspaceOrThrow();
  }

  /** @deprecated Use state.workspace.getDisplayedWorkspace() instead */
  getDisplayedWorkspace(): string {
    return this.workspace.getDisplayedWorkspace();
  }

  /** @deprecated Use state.workspace.restoreWorkspaceFromLocalStorage() instead */
  restoreWorkspaceFromLocalStorage(): string | null {
    return this.workspace.restoreWorkspaceFromLocalStorage();
  }

  /**
   * Sets the workspace resetting flag.
   * Used to show loading state during factory reset operations.
   *
   * @param isResetting - True if workspace is being reset, false otherwise
   */
  setResettingWorkspace(isResetting: boolean): void {
    this.isResettingWorkspace = isResetting;
    this.notify();
  }

  /**
   * Checks if a workspace reset operation is in progress.
   *
   * @returns True if workspace is being reset, false otherwise
   */
  isWorkspaceResetting(): boolean {
    return this.isResettingWorkspace;
  }

  /**
   * Sets the application initialization flag.
   * Used to show loading state during app startup.
   *
   * @param isInitializing - True if app is initializing, false otherwise
   */
  setInitializing(isInitializing: boolean): void {
    this.isInitializing = isInitializing;
    this.notify();
  }

  /**
   * Checks if the application is currently initializing.
   *
   * @returns True if app is initializing, false otherwise
   */
  isAppInitializing(): boolean {
    return this.isInitializing;
  }

  /**
   * Sets offline mode flag.
   *
   * @param offlineMode - True to enable offline mode (skip AI agent launches), false to disable
   */
  setOfflineMode(offlineMode: boolean): void {
    this.offlineMode = offlineMode;
    this.notify();
  }

  /**
   * Checks if offline mode is enabled.
   *
   * @returns True if offline mode is enabled, false otherwise
   */
  isOfflineMode(): boolean {
    return this.offlineMode;
  }

  // ============================================================================
  // Application-Level State Management
  // ============================================================================

  /**
   * Clears all application state except the terminal number counter.
   * Removes all terminals, workspace paths, and resets flags.
   * The terminal number counter persists to maintain monotonic numbering across workspace changes.
   */
  clearAll(): void {
    this.terminals.clearTerminals();
    this.workspace.clearWorkspace();
    // Note: Terminal number counter persists in TerminalStateManager
    this.notify();
  }

  /**
   * Registers a callback to be notified of state changes.
   * The callback will be invoked whenever any state mutation occurs.
   * This is the core of the Observer pattern implementation.
   *
   * @param callback - Function to call when state changes
   * @returns Cleanup function to unregister the callback
   *
   * @example
   * ```ts
   * const unsubscribe = appState.onChange(() => {
   *   console.log('State changed!');
   *   render();
   * });
   *
   * // Later, to stop listening:
   * unsubscribe();
   * ```
   */
  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  /**
   * Sets the auto-save callback for automatic state persistence.
   * When set, state will be automatically saved 2 seconds after the last change.
   * This implements debounced auto-save to prevent excessive file I/O.
   *
   * @param callback - Async function that saves the current state
   *
   * @example
   * ```ts
   * import { saveCurrentConfiguration } from './config';
   *
   * appState.setAutoSave(async () => {
   *   await saveCurrentConfiguration(appState);
   * });
   * ```
   */
  setAutoSave(callback: () => Promise<void>): void {
    this.autoSaveCallback = callback;
  }

  /**
   * Triggers an immediate save of the current state.
   * Bypasses the debounce timer and saves state immediately.
   * Useful for critical operations like app shutdown.
   *
   * @returns Promise that resolves when save is complete
   */
  async saveNow(): Promise<void> {
    // Cancel pending debounced save
    if (this.autoSaveTimer) {
      clearTimeout(this.autoSaveTimer);
      this.autoSaveTimer = null;
    }

    // Execute save immediately if callback is set
    if (this.autoSaveCallback) {
      await this.autoSaveCallback();
    }
  }

  private notify(): void {
    // Notify UI listeners
    this.listeners.forEach((cb) => cb());

    // Trigger debounced auto-save if enabled
    if (this.autoSaveCallback) {
      // Clear existing timer
      if (this.autoSaveTimer) {
        clearTimeout(this.autoSaveTimer);
      }

      // Set new timer for 2 seconds from now
      this.autoSaveTimer = setTimeout(() => {
        if (this.autoSaveCallback) {
          this.autoSaveCallback().catch((error) => {
            logger.error("Auto-save failed", error as Error);
          });
        }
        this.autoSaveTimer = null;
      }, 2000);
    }
  }
}

// Singleton instance for accessing state from anywhere
let appStateInstance: AppState | null = null;

/**
 * Gets the singleton AppState instance.
 * Creates a new instance if one doesn't exist.
 * Use this to access application state from anywhere in the codebase.
 *
 * @returns The singleton AppState instance
 */
export function getAppState(): AppState {
  if (!appStateInstance) {
    appStateInstance = new AppState();
  }
  return appStateInstance;
}

/**
 * Sets the singleton AppState instance.
 * Primarily used for testing to inject a mock state instance.
 *
 * @param state - The AppState instance to use as the singleton
 */
export function setAppState(state: AppState): void {
  appStateInstance = state;
}
