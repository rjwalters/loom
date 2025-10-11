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
    console.log('🔄 reorderTerminal called:', { draggedId, targetId, insertBefore });
    console.log('📊 Order before:', [...this.order]);

    const draggedIndex = this.order.indexOf(draggedId);
    const targetIndex = this.order.indexOf(targetId);

    if (draggedIndex === -1 || targetIndex === -1) {
      console.log('❌ Invalid IDs - draggedIndex:', draggedIndex, 'targetIndex:', targetIndex);
      return; // Invalid IDs
    }

    // Remove dragged terminal from current position
    this.order.splice(draggedIndex, 1);
    console.log('📊 After removing dragged:', [...this.order]);

    // Calculate new insertion index
    let newIndex = this.order.indexOf(targetId);
    if (!insertBefore) {
      newIndex++;
    }
    console.log('📍 Inserting at index:', newIndex, 'insertBefore:', insertBefore);

    // Insert at new position
    this.order.splice(newIndex, 0, draggedId);
    console.log('📊 Order after:', [...this.order]);

    this.notify();
  }

  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  private notify(): void {
    this.listeners.forEach(cb => cb());
  }
}
