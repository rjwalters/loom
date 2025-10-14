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
  id: string;
  name: string;
  status: TerminalStatus;
  isPrimary: boolean;
  role?: string; // Optional: "claude-code-worker", "codex-worker", etc. Undefined = plain shell
  roleConfig?: Record<string, unknown>; // Role-specific configuration (e.g., system prompt)
  missingSession?: boolean; // Flag for terminals with missing tmux sessions (used in error recovery)
  theme?: string; // Theme ID (e.g., "ocean", "forest") or "default"
  customTheme?: ColorTheme; // For custom colors
  // Agent-specific fields
  worktreePath?: string; // Path to git worktree
  agentPid?: number; // Claude process ID
  agentStatus?: AgentStatus; // Agent state machine
  lastIntervalRun?: number; // Timestamp (ms)
  pendingInputRequests?: InputRequest[]; // Queue of input requests
}

export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private primaryId: string | null = null;
  private order: string[] = []; // Track display order of terminal IDs
  private listeners: Set<() => void> = new Set();
  private workspacePath: string | null = null; // Valid workspace path
  private displayedWorkspacePath: string = ""; // Path shown in input (may be invalid)
  private nextAgentNumber: number = 1; // Counter for agent numbering (always increments)

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

  updateTerminalWorkerType(id: string, workerType: "claude" | "codex"): void {
    const terminal = this.terminals.get(id);
    if (terminal?.roleConfig) {
      terminal.roleConfig.workerType = workerType;
      this.notify();
    }
  }

  getPrimary(): Terminal | null {
    return this.primaryId ? this.terminals.get(this.primaryId) || null : null;
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
    this.notify();
  }

  setDisplayedWorkspace(path: string): void {
    this.displayedWorkspacePath = path;
    this.notify();
  }

  getWorkspace(): string | null {
    return this.workspacePath;
  }

  getDisplayedWorkspace(): string {
    return this.displayedWorkspacePath;
  }

  getNextAgentNumber(): number {
    return this.nextAgentNumber++;
  }

  setNextAgentNumber(num: number): void {
    this.nextAgentNumber = num;
  }

  getCurrentAgentNumber(): number {
    return this.nextAgentNumber;
  }

  loadAgents(agents: Terminal[]): void {
    // Clear existing terminals
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;

    // Add each terminal
    agents.forEach((agent) => {
      this.addTerminal(agent);
    });
  }

  clearAll(): void {
    // Clear all state
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;
    this.workspacePath = null;
    this.displayedWorkspacePath = "";
    // Note: Don't reset nextAgentNumber - it persists across workspace changes
    this.notify();
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
