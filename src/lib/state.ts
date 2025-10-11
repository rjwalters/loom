export enum TerminalStatus {
  Idle = 'idle',
  Busy = 'busy',
  NeedsInput = 'needs_input',
  Error = 'error',
  Stopped = 'stopped'
}

export interface Terminal {
  id: string;
  name: string;
  status: TerminalStatus;
  isPrimary: boolean;
}

export class AppState {
  private terminals: Map<string, Terminal> = new Map();
  private primaryId: string | null = null;
  private order: string[] = []; // Track display order of terminal IDs
  private listeners: Set<() => void> = new Set();
  private workspacePath: string | null = null; // Valid workspace path
  private displayedWorkspacePath: string = ''; // Path shown in input (may be invalid)

  addTerminal(terminal: Terminal): void {
    this.terminals.set(terminal.id, terminal);
    this.order.push(terminal.id); // Add to end of order
    if (terminal.isPrimary) {
      this.primaryId = terminal.id;
    }
    this.notify();
  }

  removeTerminal(id: string): void {
    // Don't allow removing the last terminal
    if (this.terminals.size <= 1) {
      return;
    }

    this.terminals.delete(id);
    this.order = this.order.filter(tid => tid !== id); // Remove from order

    // If we removed the primary, make the first remaining terminal primary
    if (this.primaryId === id) {
      const firstId = this.order[0];
      if (firstId) {
        this.setPrimary(firstId);
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

  getPrimary(): Terminal | null {
    return this.primaryId ? this.terminals.get(this.primaryId) || null : null;
  }

  getTerminals(): Terminal[] {
    // Return terminals in display order
    return this.order
      .map(id => this.terminals.get(id))
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

  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  private notify(): void {
    this.listeners.forEach(cb => cb());
  }
}
