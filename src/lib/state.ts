import { Logger } from "./logger";

const logger = Logger.forComponent("state");

export enum TerminalStatus {
  Idle = "idle",
  Busy = "busy",
  NeedsInput = "needs_input",
  Error = "error",
  Stopped = "stopped",
}

export enum AgentStatus {
  NotStarted = "not_started",
  Initializing = "initializing",
  Ready = "ready",
  Busy = "busy",
  WaitingForInput = "waiting_for_input",
  Error = "error",
  Stopped = "stopped",
}

export interface ColorTheme {
  name: string;
  primary: string;
  background?: string;
  border: string;
}

export interface InputRequest {
  id: string; // Unique request ID
  prompt: string; // Question from Claude
  timestamp: number; // When requested (ms)
}

export interface Terminal {
  id: string; // Stable terminal ID (e.g., "terminal-1"), used for both config and runtime
  name: string;
  status: TerminalStatus;
  isPrimary: boolean;
  role?: string; // Optional: "claude-code-worker", "codex-worker", etc. Undefined = plain shell
  roleConfig?: Record<string, unknown>; // Role-specific configuration (e.g., system prompt)
  missingSession?: boolean; // Flag for terminals with missing tmux sessions (used in error recovery)
  theme?: string; // Theme ID (e.g., "ocean", "forest") or "default"
  customTheme?: ColorTheme; // For custom colors
  // Agent-specific fields
  worktreePath?: string; // Path to git worktree (automatically created at .loom/worktrees/{id})
  agentPid?: number; // Claude process ID
  agentStatus?: AgentStatus; // Agent state machine
  lastIntervalRun?: number; // Timestamp (ms)
  pendingInputRequests?: InputRequest[]; // Queue of input requests
  // Timer tracking fields
  busyTime?: number; // Total milliseconds spent in busy state
  idleTime?: number; // Total milliseconds spent in idle state
  lastStateChange?: number; // Timestamp (ms) of last status change
}

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

  addTerminal(terminal: Terminal): void {
    this.terminals.set(terminal.id, terminal);
    this.order.push(terminal.id); // Add to end of order
    if (terminal.isPrimary) {
      this.primaryId = terminal.id;
    }
    this.notify();
  }

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

  renameTerminal(id: string, newName: string): void {
    const terminal = this.terminals.get(id);
    if (terminal && newName.trim()) {
      terminal.name = newName.trim();
      this.notify();
    }
  }

  updateTerminal(id: string, updates: Partial<Terminal>): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      Object.assign(terminal, updates);
      this.notify();
    }
  }

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

  setTerminalTheme(id: string, themeId: string): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.theme = themeId;
      delete terminal.customTheme; // Clear custom if using preset
      this.notify();
    }
  }

  setTerminalCustomTheme(id: string, theme: ColorTheme): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.theme = "custom";
      terminal.customTheme = theme;
      this.notify();
    }
  }

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

  getPrimary(): Terminal | null {
    return this.primaryId ? this.terminals.get(this.primaryId) || null : null;
  }

  /**
   * Check if a primary terminal exists
   */
  hasPrimary(): boolean {
    return this.primaryId !== null && this.terminals.has(this.primaryId);
  }

  /**
   * Get primary terminal or throw error if none exists
   * Use this when you're certain a primary must exist (e.g., after validation)
   */
  getPrimaryOrThrow(): Terminal {
    const primary = this.getPrimary();
    if (!primary) {
      throw new Error("No primary terminal available");
    }
    return primary;
  }

  getTerminals(): Terminal[] {
    // Return terminals in display order
    return this.order
      .map((id) => this.terminals.get(id))
      .filter((t): t is Terminal => t !== undefined);
  }

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

  setDisplayedWorkspace(path: string): void {
    this.displayedWorkspacePath = path;
    this.notify();
  }

  getWorkspace(): string | null {
    return this.workspacePath;
  }

  /**
   * Check if a valid workspace is set
   */
  hasWorkspace(): boolean {
    return this.workspacePath !== null && this.workspacePath !== "";
  }

  /**
   * Get workspace path or throw error if none exists
   * Use this when workspace is required for an operation
   */
  getWorkspaceOrThrow(): string {
    if (!this.workspacePath) {
      throw new Error("No workspace selected");
    }
    return this.workspacePath;
  }

  getDisplayedWorkspace(): string {
    return this.displayedWorkspacePath;
  }

  setResettingWorkspace(isResetting: boolean): void {
    this.isResettingWorkspace = isResetting;
    this.notify();
  }

  isWorkspaceResetting(): boolean {
    return this.isResettingWorkspace;
  }

  setInitializing(isInitializing: boolean): void {
    this.isInitializing = isInitializing;
    this.notify();
  }

  isAppInitializing(): boolean {
    return this.isInitializing;
  }

  getNextTerminalNumber(): number {
    return this.nextTerminalNumber++;
  }

  setNextTerminalNumber(num: number): void {
    this.nextTerminalNumber = num;
  }

  getCurrentTerminalNumber(): number {
    return this.nextTerminalNumber;
  }

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
   * Restore workspace from localStorage (for HMR survival)
   * Returns the restored workspace path or null if none was stored
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

  // Helper method to get terminal by ID
  getTerminal(id: string): Terminal | null {
    return this.terminals.get(id) || null;
  }

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

export function getAppState(): AppState {
  if (!appStateInstance) {
    appStateInstance = new AppState();
  }
  return appStateInstance;
}

export function setAppState(state: AppState): void {
  appStateInstance = state;
}

/**
 * Type guard to check if a value is a valid Terminal
 * Useful for filtering and narrowing types
 */
export function isValidTerminal(t: Terminal | null | undefined): t is Terminal {
  return t !== null && t !== undefined && typeof t.id === "string" && t.id.length > 0;
}
