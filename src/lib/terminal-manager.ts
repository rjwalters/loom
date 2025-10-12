import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

export interface ManagedTerminal {
  terminal: Terminal;
  fitAddon: FitAddon;
  container: HTMLElement;
  attached: boolean;
}

export class TerminalManager {
  private terminals: Map<string, ManagedTerminal> = new Map();

  /**
   * Create a new xterm.js terminal instance and attach it to a DOM element
   */
  createTerminal(terminalId: string, containerId: string): ManagedTerminal | null {
    const container = document.getElementById(containerId);
    if (!container) {
      console.error(`Container ${containerId} not found`);
      return null;
    }

    // Check if terminal already exists
    const existing = this.terminals.get(terminalId);
    if (existing) {
      console.warn(`Terminal ${terminalId} already exists`);
      return existing;
    }

    // Create xterm.js Terminal instance with fixed size matching tmux
    const terminal = new Terminal({
      cols: 120, // Fixed width to match tmux
      rows: 30, // Fixed height to match tmux
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      theme: {
        background: "#1e1e1e",
        foreground: "#d4d4d4",
        cursor: "#ffffff",
        selectionBackground: "rgba(255, 255, 255, 0.3)",
        black: "#000000",
        red: "#cd3131",
        green: "#0dbc79",
        yellow: "#e5e510",
        blue: "#2472c8",
        magenta: "#bc3fbc",
        cyan: "#11a8cd",
        white: "#e5e5e5",
        brightBlack: "#666666",
        brightRed: "#f14c4c",
        brightGreen: "#23d18b",
        brightYellow: "#f5f543",
        brightBlue: "#3b8eea",
        brightMagenta: "#d670d6",
        brightCyan: "#29b8db",
        brightWhite: "#e5e5e5",
      },
      allowProposedApi: true,
      scrollback: 10000, // Keep plenty of scrollback
    });

    // Create and load addons
    const fitAddon = new FitAddon(); // Keep for compatibility but don't use for resizing
    const webLinksAddon = new WebLinksAddon();

    terminal.loadAddon(fitAddon);
    terminal.loadAddon(webLinksAddon);

    // Try to load WebGL addon (fallback if it fails)
    try {
      const webglAddon = new WebglAddon();
      terminal.loadAddon(webglAddon);
    } catch (e) {
      console.warn("WebGL addon failed to load, using canvas renderer", e);
    }

    // Open terminal in container
    terminal.open(container);

    // Store managed terminal
    const managedTerminal: ManagedTerminal = {
      terminal,
      fitAddon,
      container,
      attached: false,
    };
    this.terminals.set(terminalId, managedTerminal);

    // No resize needed - using fixed size that matches tmux session

    return managedTerminal;
  }

  /**
   * Get a managed terminal by ID
   */
  getTerminal(terminalId: string): ManagedTerminal | undefined {
    return this.terminals.get(terminalId);
  }

  /**
   * Write data to a terminal
   */
  writeToTerminal(terminalId: string, data: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    managed.terminal.write(data);
  }

  /**
   * Clear terminal and write new content (for full-state updates)
   */
  clearAndWriteTerminal(terminalId: string, data: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    // Clear the terminal display
    managed.terminal.clear();

    // Reset cursor to home position
    managed.terminal.write("\x1b[H");

    // Write the full terminal state
    managed.terminal.write(data);
  }

  /**
   * Clear a terminal's output
   */
  clearTerminal(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    managed.terminal.clear();
  }

  /**
   * Fit terminal to its container size (no-op for fixed size terminals)
   * Kept for API compatibility
   */
  async fitTerminal(terminalId: string): Promise<void> {
    // No-op: using fixed terminal size
    console.log(`[fitTerminal] Skipping resize for ${terminalId} (using fixed size)`);
  }

  /**
   * Fit all terminals (no-op for fixed size terminals)
   * Kept for API compatibility
   */
  fitAllTerminals(): void {
    // No-op: using fixed terminal size
  }

  /**
   * Destroy a terminal instance and clean up resources
   */
  destroyTerminal(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      return;
    }

    // Dispose of the terminal
    managed.terminal.dispose();

    // Remove from map
    this.terminals.delete(terminalId);
  }

  /**
   * Mark a terminal as attached to daemon
   */
  markAttached(terminalId: string, attached: boolean): void {
    const managed = this.terminals.get(terminalId);
    if (managed) {
      managed.attached = attached;
    }
  }

  /**
   * Check if a terminal is attached to daemon
   */
  isAttached(terminalId: string): boolean {
    const managed = this.terminals.get(terminalId);
    return managed?.attached ?? false;
  }

  /**
   * Update terminal theme based on dark/light mode
   */
  updateTheme(terminalId: string, isDark: boolean): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      return;
    }

    const theme = isDark
      ? {
          background: "#1e1e1e",
          foreground: "#d4d4d4",
          cursor: "#ffffff",
          selectionBackground: "rgba(255, 255, 255, 0.3)",
        }
      : {
          background: "#ffffff",
          foreground: "#333333",
          cursor: "#000000",
          selectionBackground: "rgba(0, 0, 0, 0.3)",
        };

    managed.terminal.options.theme = theme;
  }

  /**
   * Update all terminals' themes
   */
  updateAllThemes(isDark: boolean): void {
    for (const [id] of this.terminals) {
      this.updateTheme(id, isDark);
    }
  }

  /**
   * Get all terminal IDs
   */
  getTerminalIds(): string[] {
    return Array.from(this.terminals.keys());
  }

  /**
   * Get count of managed terminals
   */
  getTerminalCount(): number {
    return this.terminals.size;
  }

  /**
   * Destroy all terminals
   */
  destroyAll(): void {
    for (const [id] of this.terminals) {
      this.destroyTerminal(id);
    }
  }
}

// Singleton instance
let terminalManagerInstance: TerminalManager | null = null;

export function getTerminalManager(): TerminalManager {
  if (!terminalManagerInstance) {
    terminalManagerInstance = new TerminalManager();
  }
  return terminalManagerInstance;
}
