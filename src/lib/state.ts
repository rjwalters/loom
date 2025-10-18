import { Logger } from "./logger";

const logger = Logger.forComponent("state");

/**
 * Represents the operational status of a terminal.
 * Used to track the current state of terminal activity.
 */
export enum TerminalStatus {
  /** Terminal is idle and ready for new tasks */
  Idle = "idle",
  /** Terminal is actively executing a task */
  Busy = "busy",
  /** Terminal is waiting for user input */
  NeedsInput = "needs_input",
  /** Terminal has encountered an error */
  Error = "error",
  /** Terminal has been stopped */
  Stopped = "stopped",
}

/**
 * Represents the status of an AI agent running in a terminal.
 * Tracks the agent's lifecycle from initialization through execution.
 */
export enum AgentStatus {
  /** Agent has not been started yet */
  NotStarted = "not_started",
  /** Agent is initializing (spawning process, loading context) */
  Initializing = "initializing",
  /** Agent is ready and waiting for work */
  Ready = "ready",
  /** Agent is actively working on a task */
  Busy = "busy",
  /** Agent is waiting for user input */
  WaitingForInput = "waiting_for_input",
  /** Agent has encountered an error */
  Error = "error",
  /** Agent has been stopped */
  Stopped = "stopped",
}

/**
 * Defines a custom color theme for a terminal.
 * Used to personalize terminal appearance beyond preset themes.
 */
export interface ColorTheme {
  /** Display name of the theme */
  name: string;
  /** Primary color (hex or CSS color) */
  primary: string;
  /** Optional background color (hex or CSS color) */
  background?: string;
  /** Border color (hex or CSS color) */
  border: string;
}

/**
 * Represents a pending input request from an AI agent.
 * Agents can queue multiple input requests when they need user interaction.
 */
export interface InputRequest {
  /** Unique identifier for this input request */
  id: string;
  /** The question or prompt from the agent */
  prompt: string;
  /** Unix timestamp (ms) when the request was created */
  timestamp: number;
}

/**
 * Represents a terminal instance in the application.
 * Terminals can be plain shells or AI agents with specialized roles.
 */
export interface Terminal {
  /** Stable terminal identifier (e.g., "terminal-1") used for both config and runtime */
  id: string;
  /** User-friendly display name */
  name: string;
  /** Current operational status */
  status: TerminalStatus;
  /** Whether this terminal is currently selected as the primary view */
  isPrimary: boolean;
  /** Optional role identifier (e.g., "claude-code-worker"). Undefined = plain shell */
  role?: string;
  /** Role-specific configuration data (e.g., system prompt, worker type) */
  roleConfig?: Record<string, unknown>;
  /** Flag indicating the tmux session is missing (used in error recovery) */
  missingSession?: boolean;
  /** Theme identifier (e.g., "ocean", "forest") or "default" */
  theme?: string;
  /** Custom color theme configuration */
  customTheme?: ColorTheme;
  /** Path to git worktree (automatically created at .loom/worktrees/{id}) */
  worktreePath?: string;
  /** Process ID of the running agent */
  agentPid?: number;
  /** Current agent lifecycle status */
  agentStatus?: AgentStatus;
  /** Unix timestamp (ms) of the last autonomous interval execution */
  lastIntervalRun?: number;
  /** Queue of pending input requests from the agent */
  pendingInputRequests?: InputRequest[];
  /** Total milliseconds spent in busy state (for analytics) */
  busyTime?: number;
  /** Total milliseconds spent in idle state (for analytics) */
  idleTime?: number;
  /** Unix timestamp (ms) of the last status change */
  lastStateChange?: number;
}

/**
 * Central state management for the Loom application.
 * Implements the Observer pattern to automatically update UI when state changes.
 *
 * This class manages:
 * - Terminal instances and their lifecycle
 * - Primary terminal selection
 * - Terminal display order
 * - Workspace path validation
 * - Application initialization state
 *
 * All state mutations automatically notify registered listeners via the onChange() callback system.
 */
export class AppState {
  private terminals: Map<string, Terminal> = new Map(); // Key is terminal id
  private primaryId: string | null = null; // Store primary terminal id
  private order: string[] = []; // Track display order of terminal ids
  private listeners: Set<() => void> = new Set();
  private workspacePath: string | null = null; // Valid workspace path
  private displayedWorkspacePath: string = ""; // Path shown in input (may be invalid)
  private nextTerminalNumber: number = 1; // Counter for terminal numbering (always increments)
  private isResettingWorkspace: boolean = false; // Loading state during factory reset
  private isInitializing: boolean = false; // Loading state during app startup

  /**
   * Adds a new terminal to the application state.
   * Automatically assigns it as primary if it's marked as primary in the terminal object.
   * Adds the terminal to the end of the display order.
   *
   * @param terminal - The terminal configuration to add
   */
  addTerminal(terminal: Terminal): void {
    this.terminals.set(terminal.id, terminal);
    this.order.push(terminal.id); // Add to end of order
    if (terminal.isPrimary) {
      this.primaryId = terminal.id;
    }
    this.notify();
  }

  /**
   * Removes a terminal from the application state.
   * If the removed terminal was primary, automatically promotes the first remaining terminal.
   *
   * @param id - The terminal ID to remove
   */
  removeTerminal(id: string): void {
    this.terminals.delete(id);
    this.order = this.order.filter((tid) => tid !== id); // Remove from order

    // If we removed the primary, make the first remaining terminal primary (if any)
    if (this.primaryId === id) {
      const firstId = this.order[0];
      if (firstId) {
        this.setPrimary(firstId);
      } else {
        // No terminals left - clear primary
        this.primaryId = null;
      }
    }

    this.notify();
  }

  /**
   * Sets the specified terminal as the primary (selected) terminal.
   * Automatically clears the primary flag from the previously selected terminal.
   *
   * @param id - The terminal ID to make primary
   */
  setPrimary(id: string): void {
    // Clear old primary
    if (this.primaryId) {
      const old = this.terminals.get(this.primaryId);
      if (old) {
        old.isPrimary = false;
      }
    }

    // Set new primary
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.isPrimary = true;
      this.primaryId = id;
      this.notify();
    }
  }

  /**
   * Renames a terminal with validation to ensure non-empty names.
   * Trims whitespace from the new name.
   *
   * @param id - The terminal ID to rename
   * @param newName - The new name (will be trimmed)
   */
  renameTerminal(id: string, newName: string): void {
    const terminal = this.terminals.get(id);
    if (terminal && newName.trim()) {
      terminal.name = newName.trim();
      this.notify();
    }
  }

  /**
   * Updates a terminal with partial changes.
   * Useful for updating multiple properties at once or properties not covered by specific setters.
   *
   * @param id - The terminal ID to update
   * @param updates - Partial terminal object with properties to update
   */
  updateTerminal(id: string, updates: Partial<Terminal>): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      Object.assign(terminal, updates);
      this.notify();
    }
  }

  /**
   * Updates a terminal's status and tracks time spent in each state.
   * Automatically calculates elapsed time in previous status and updates busyTime/idleTime counters.
   *
   * @param id - The terminal ID to update
   * @param newStatus - The new status to set
   */
  updateTerminalStatus(id: string, newStatus: TerminalStatus): void {
    const terminal = this.terminals.get(id);
    if (!terminal) {
      return;
    }

    const now = Date.now();
    const oldStatus = terminal.status;

    // Only process timer updates if status actually changed
    if (oldStatus !== newStatus && terminal.lastStateChange) {
      const elapsed = now - terminal.lastStateChange;

      // Add elapsed time to the appropriate counter
      if (oldStatus === TerminalStatus.Busy) {
        terminal.busyTime = (terminal.busyTime || 0) + elapsed;
      } else if (oldStatus === TerminalStatus.Idle) {
        terminal.idleTime = (terminal.idleTime || 0) + elapsed;
      }
    }

    // Update status and timestamp
    terminal.status = newStatus;
    terminal.lastStateChange = now;

    // Initialize timers if this is the first state change
    if (terminal.busyTime === undefined) {
      terminal.busyTime = 0;
    }
    if (terminal.idleTime === undefined) {
      terminal.idleTime = 0;
    }

    this.notify();
  }

  /**
   * Sets or clears a terminal's role and role configuration.
   * Roles define specialized behavior for AI agents (e.g., "claude-code-worker", "reviewer").
   *
   * @param id - The terminal ID to update
   * @param role - The role identifier, or undefined to clear the role
   * @param roleConfig - Optional role-specific configuration object
   */
  setTerminalRole(
    id: string,
    role: string | undefined,
    roleConfig?: Record<string, unknown>
  ): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.role = role;
      terminal.roleConfig = roleConfig;
      this.notify();
    }
  }

  /**
   * Sets a terminal's theme to a preset theme by ID.
   * Clears any custom theme configuration when using a preset.
   *
   * @param id - The terminal ID to update
   * @param themeId - The preset theme identifier (e.g., "ocean", "forest", "default")
   */
  setTerminalTheme(id: string, themeId: string): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.theme = themeId;
      delete terminal.customTheme; // Clear custom if using preset
      this.notify();
    }
  }

  /**
   * Sets a terminal's theme to a custom color scheme.
   * Automatically sets the theme ID to "custom".
   *
   * @param id - The terminal ID to update
   * @param theme - The custom color theme configuration
   */
  setTerminalCustomTheme(id: string, theme: ColorTheme): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.theme = "custom";
      terminal.customTheme = theme;
      this.notify();
    }
  }

  /**
   * Updates the worker type for a terminal that has a role configuration.
   * Used to switch between different AI providers (Claude, Codex, etc.).
   *
   * @param id - The terminal ID to update
   * @param workerType - The AI worker type to use
   */
  updateTerminalWorkerType(
    id: string,
    workerType: "claude" | "codex" | "github-copilot" | "gemini" | "deepseek" | "grok"
  ): void {
    const terminal = this.terminals.get(id);
    if (terminal?.roleConfig) {
      terminal.roleConfig.workerType = workerType;
      this.notify();
    }
  }

  /**
   * Gets the current primary (selected) terminal.
   *
   * @returns The primary terminal, or null if no primary is set
   */
  getPrimary(): Terminal | null {
    return this.primaryId ? this.terminals.get(this.primaryId) || null : null;
  }

  /**
   * Check if a primary terminal exists.
   *
   * @returns True if a valid primary terminal is set, false otherwise
   */
  hasPrimary(): boolean {
    return this.primaryId !== null && this.terminals.has(this.primaryId);
  }

  /**
   * Get primary terminal or throw error if none exists.
   * Use this when you're certain a primary must exist (e.g., after validation).
   *
   * @returns The primary terminal
   * @throws {Error} If no primary terminal is available
   */
  getPrimaryOrThrow(): Terminal {
    const primary = this.getPrimary();
    if (!primary) {
      throw new Error("No primary terminal available");
    }
    return primary;
  }

  /**
   * Gets all terminals in their current display order.
   * The order can be modified via drag-and-drop (see reorderTerminal).
   *
   * @returns Array of terminals in display order
   */
  getTerminals(): Terminal[] {
    // Return terminals in display order
    return this.order
      .map((id) => this.terminals.get(id))
      .filter((t): t is Terminal => t !== undefined);
  }

  /**
   * Reorders a terminal in the display sequence via drag-and-drop.
   * Used to implement drag-and-drop reordering in the mini terminal row.
   *
   * @param draggedId - The ID of the terminal being dragged
   * @param targetId - The ID of the terminal being dropped onto
   * @param insertBefore - If true, insert before target; if false, insert after target
   */
  reorderTerminal(draggedId: string, targetId: string, insertBefore: boolean): void {
    const draggedIndex = this.order.indexOf(draggedId);
    const targetIndex = this.order.indexOf(targetId);

    if (draggedIndex === -1 || targetIndex === -1) {
      return; // Invalid IDs
    }

    // Remove dragged terminal from current position
    this.order.splice(draggedIndex, 1);

    // Calculate new insertion index
    let newIndex = this.order.indexOf(targetId);
    if (!insertBefore) {
      newIndex++;
    }

    // Insert at new position
    this.order.splice(newIndex, 0, draggedId);

    this.notify();
  }

  /**
   * Sets the validated workspace path and persists it to localStorage.
   * Also updates the displayed workspace path to match.
   * Workspace path persists across HMR reloads.
   *
   * @param path - The validated workspace path, or empty string to clear
   */
  setWorkspace(path: string): void {
    this.workspacePath = path;
    this.displayedWorkspacePath = path;
    // Persist workspace to localStorage to survive HMR reloads
    if (path) {
      localStorage.setItem("loom:workspace", path);
    } else {
      localStorage.removeItem("loom:workspace");
    }
    this.notify();
  }

  /**
   * Sets the displayed workspace path without validation.
   * Used to show user input in the workspace selector even if it's invalid.
   * This allows showing specific error messages while preserving user typing.
   *
   * @param path - The path to display (may be invalid)
   */
  setDisplayedWorkspace(path: string): void {
    this.displayedWorkspacePath = path;
    this.notify();
  }

  /**
   * Gets the current validated workspace path.
   *
   * @returns The workspace path, or null if no valid workspace is set
   */
  getWorkspace(): string | null {
    return this.workspacePath;
  }

  /**
   * Check if a valid workspace is set.
   *
   * @returns True if a non-empty workspace path is set, false otherwise
   */
  hasWorkspace(): boolean {
    return this.workspacePath !== null && this.workspacePath !== "";
  }

  /**
   * Get workspace path or throw error if none exists.
   * Use this when workspace is required for an operation.
   *
   * @returns The workspace path
   * @throws {Error} If no workspace is selected
   */
  getWorkspaceOrThrow(): string {
    if (!this.workspacePath) {
      throw new Error("No workspace selected");
    }
    return this.workspacePath;
  }

  /**
   * Gets the displayed workspace path (which may be invalid).
   * This may differ from getWorkspace() when user has entered an invalid path.
   *
   * @returns The displayed workspace path
   */
  getDisplayedWorkspace(): string {
    return this.displayedWorkspacePath;
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
   * Gets the next available terminal number and increments the counter.
   * Terminal numbering is monotonic and never reused, even after terminal deletion.
   *
   * @returns The next terminal number
   */
  getNextTerminalNumber(): number {
    return this.nextTerminalNumber++;
  }

  /**
   * Sets the terminal number counter to a specific value.
   * Used when loading configuration from disk to restore the counter state.
   *
   * @param num - The terminal number to set
   */
  setNextTerminalNumber(num: number): void {
    this.nextTerminalNumber = num;
  }

  /**
   * Gets the current terminal number without incrementing.
   * Useful for displaying the current counter value or saving configuration.
   *
   * @returns The current terminal number
   */
  getCurrentTerminalNumber(): number {
    return this.nextTerminalNumber;
  }

  /**
   * Loads a complete set of terminals from configuration.
   * Clears all existing terminals and replaces them with the provided ones.
   * Automatically sets a primary terminal if none is marked as primary.
   *
   * @param agents - Array of terminal configurations to load
   */
  loadAgents(agents: Terminal[]): void {
    // Clear existing terminals
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;

    // Add each terminal
    agents.forEach((agent) => {
      // Check if agent has id
      if (!agent.id) {
        logger.warn("Skipping terminal without id", {
          terminalName: agent.name,
        });
        return;
      }
      this.addTerminal(agent);
    });

    // If no terminal was marked as primary, make the first one primary
    if (!this.primaryId && this.order.length > 0) {
      const firstId = this.order[0];
      logger.info("No primary terminal set, making first terminal primary", {
        terminalId: firstId,
      });
      this.setPrimary(firstId);
    }
  }

  /**
   * Clears all application state except the terminal number counter.
   * Removes all terminals, workspace paths, and resets flags.
   * The terminal number counter persists to maintain monotonic numbering across workspace changes.
   */
  clearAll(): void {
    // Clear all state
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;
    this.workspacePath = null;
    this.displayedWorkspacePath = "";
    // Note: Don't reset nextTerminalNumber - it persists across workspace changes
    this.notify();
  }

  /**
   * Restore workspace from localStorage (for HMR survival).
   * Workspace path is automatically persisted to survive hot module replacement during development.
   *
   * @returns The restored workspace path, or null if none was stored
   */
  restoreWorkspaceFromLocalStorage(): string | null {
    const stored = localStorage.getItem("loom:workspace");
    if (stored) {
      this.workspacePath = stored;
      this.displayedWorkspacePath = stored;
      this.notify();
      return stored;
    }
    return null;
  }

  /**
   * Gets a terminal by its ID.
   *
   * @param id - The terminal ID to look up
   * @returns The terminal if found, null otherwise
   */
  getTerminal(id: string): Terminal | null {
    return this.terminals.get(id) || null;
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

  private notify(): void {
    this.listeners.forEach((cb) => cb());
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

/**
 * Type guard to check if a value is a valid Terminal.
 * Useful for filtering and narrowing types in array operations.
 *
 * @param t - The value to check
 * @returns True if the value is a valid Terminal with a non-empty ID
 *
 * @example
 * ```ts
 * const terminals = [terminal1, null, terminal2, undefined];
 * const validTerminals = terminals.filter(isValidTerminal);
 * // validTerminals is now Terminal[] (not (Terminal | null | undefined)[])
 * ```
 */
export function isValidTerminal(t: Terminal | null | undefined): t is Terminal {
  return t !== null && t !== undefined && typeof t.id === "string" && t.id.length > 0;
}
