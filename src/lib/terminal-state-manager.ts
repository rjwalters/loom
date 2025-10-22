import { Logger } from "./logger";
import type { Terminal, TerminalStatus, ColorTheme } from "./types";

const logger = Logger.forComponent("terminal-state-manager");

/**
 * Manages terminal state: instances, primary selection, ordering, and configuration.
 * Handles terminal CRUD operations, role/theme configuration, and display ordering.
 */
export class TerminalStateManager {
  private terminals: Map<string, Terminal> = new Map();
  private primaryId: string | null = null;
  private order: string[] = [];
  private listeners: Set<() => void> = new Set();
  private nextTerminalNumber: number = 1;

  /**
   * Adds a new terminal to the manager.
   * Automatically assigns it as primary if it's marked as primary in the terminal object.
   * Adds the terminal to the end of the display order.
   */
  addTerminal(terminal: Terminal): void {
    this.terminals.set(terminal.id, terminal);
    this.order.push(terminal.id);
    if (terminal.isPrimary) {
      this.primaryId = terminal.id;
    }
    this.notify();
  }

  /**
   * Removes a terminal from the manager.
   * If the removed terminal was primary, automatically promotes the first remaining terminal.
   */
  removeTerminal(id: string): void {
    this.terminals.delete(id);
    this.order = this.order.filter((tid) => tid !== id);

    // If we removed the primary, make the first remaining terminal primary (if any)
    if (this.primaryId === id) {
      const firstId = this.order[0];
      if (firstId) {
        this.setPrimary(firstId);
      } else {
        this.primaryId = null;
      }
    }

    this.notify();
  }

  /**
   * Sets the specified terminal as the primary (selected) terminal.
   * Automatically clears the primary flag from the previously selected terminal.
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
      if (oldStatus === "busy") {
        terminal.busyTime = (terminal.busyTime || 0) + elapsed;
      } else if (oldStatus === "idle") {
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
   * Roles define specialized behavior for AI agents.
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
   */
  setTerminalTheme(id: string, themeId: string): void {
    const terminal = this.terminals.get(id);
    if (terminal) {
      terminal.theme = themeId;
      delete terminal.customTheme;
      this.notify();
    }
  }

  /**
   * Sets a terminal's theme to a custom color scheme.
   * Automatically sets the theme ID to "custom".
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
   * Used to switch between different AI providers.
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
   */
  getPrimary(): Terminal | null {
    return this.primaryId ? this.terminals.get(this.primaryId) || null : null;
  }

  /**
   * Check if a primary terminal exists.
   */
  hasPrimary(): boolean {
    return this.primaryId !== null && this.terminals.has(this.primaryId);
  }

  /**
   * Get primary terminal or throw error if none exists.
   * Use this when you're certain a primary must exist.
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
   * The order can be modified via drag-and-drop.
   */
  getTerminals(): Terminal[] {
    return this.order
      .map((id) => this.terminals.get(id))
      .filter((t): t is Terminal => t !== undefined);
  }

  /**
   * Reorders a terminal in the display sequence via drag-and-drop.
   * Used to implement drag-and-drop reordering in the mini terminal row.
   */
  reorderTerminal(draggedId: string, targetId: string, insertBefore: boolean): void {
    const draggedIndex = this.order.indexOf(draggedId);
    const targetIndex = this.order.indexOf(targetId);

    if (draggedIndex === -1 || targetIndex === -1) {
      return;
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
   * Gets a terminal by its ID.
   */
  getTerminal(id: string): Terminal | null {
    return this.terminals.get(id) || null;
  }

  /**
   * Loads a complete set of terminals from configuration.
   * Clears all existing terminals and replaces them with the provided ones.
   * Automatically sets a primary terminal if none is marked as primary.
   */
  loadTerminals(terminals: Terminal[]): void {
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;

    terminals.forEach((terminal) => {
      if (!terminal.id) {
        logger.warn("Skipping terminal without id", {
          terminalName: terminal.name,
        });
        return;
      }
      this.addTerminal(terminal);
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
   * Clears all terminals.
   * Note: Does not reset the terminal number counter.
   */
  clearTerminals(): void {
    this.terminals.clear();
    this.order = [];
    this.primaryId = null;
    this.notify();
  }

  /**
   * Gets the next available terminal number and increments the counter.
   * Terminal numbering is monotonic and never reused, even after terminal deletion.
   */
  getNextTerminalNumber(): number {
    return this.nextTerminalNumber++;
  }

  /**
   * Sets the terminal number counter to a specific value.
   * Used when loading configuration from disk to restore the counter state.
   */
  setNextTerminalNumber(num: number): void {
    this.nextTerminalNumber = num;
  }

  /**
   * Gets the current terminal number without incrementing.
   * Useful for displaying the current counter value or saving configuration.
   */
  getCurrentTerminalNumber(): number {
    return this.nextTerminalNumber;
  }

  /**
   * Registers a callback to be notified of state changes.
   */
  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  private notify(): void {
    this.listeners.forEach((cb) => cb());
  }
}
