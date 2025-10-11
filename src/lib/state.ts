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
  private listeners: Set<() => void> = new Set();

  addTerminal(terminal: Terminal): void {
    this.terminals.set(terminal.id, terminal);
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

    // If we removed the primary, make the first remaining terminal primary
    if (this.primaryId === id) {
      const first = Array.from(this.terminals.values())[0];
      if (first) {
        this.setPrimary(first.id);
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
    return Array.from(this.terminals.values());
  }

  onChange(callback: () => void): () => void {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  private notify(): void {
    this.listeners.forEach(cb => cb());
  }
}
