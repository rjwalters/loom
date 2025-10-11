import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { WebglAddon } from '@xterm/addon-webgl';
import '@xterm/xterm/css/xterm.css';

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
    if (this.terminals.has(terminalId)) {
      console.warn(`Terminal ${terminalId} already exists`);
      return this.terminals.get(terminalId)!;
    }

    // Create xterm.js Terminal instance
    const terminal = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      theme: {
        background: '#1e1e1e',
        foreground: '#d4d4d4',
        cursor: '#ffffff',
        selectionBackground: 'rgba(255, 255, 255, 0.3)',
        black: '#000000',
        red: '#cd3131',
        green: '#0dbc79',
        yellow: '#e5e510',
        blue: '#2472c8',
        magenta: '#bc3fbc',
        cyan: '#11a8cd',
        white: '#e5e5e5',
        brightBlack: '#666666',
        brightRed: '#f14c4c',
        brightGreen: '#23d18b',
        brightYellow: '#f5f543',
        brightBlue: '#3b8eea',
        brightMagenta: '#d670d6',
        brightCyan: '#29b8db',
        brightWhite: '#e5e5e5',
      },
      allowProposedApi: true,
    });

    // Create and load addons
    const fitAddon = new FitAddon();
    const webLinksAddon = new WebLinksAddon();

    terminal.loadAddon(fitAddon);
    terminal.loadAddon(webLinksAddon);

    // Try to load WebGL addon (fallback if it fails)
    try {
      const webglAddon = new WebglAddon();
      terminal.loadAddon(webglAddon);
    } catch (e) {
      console.warn('WebGL addon failed to load, using canvas renderer', e);
    }

    // Open terminal in container
    terminal.open(container);

    // Fit terminal to container size
    fitAddon.fit();

    // Store managed terminal
    const managedTerminal: ManagedTerminal = {
      terminal,
      fitAddon,
      container,
      attached: false,
    };
    this.terminals.set(terminalId, managedTerminal);

    // Set up resize observer
    this.setupResizeObserver(container, fitAddon);

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
   * Fit terminal to its container size
   */
  fitTerminal(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    try {
      managed.fitAddon.fit();
    } catch (e) {
      console.error(`Failed to fit terminal ${terminalId}:`, e);
    }
  }

  /**
   * Fit all terminals to their container sizes
   */
  fitAllTerminals(): void {
    for (const [id] of this.terminals) {
      this.fitTerminal(id);
    }
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
   * Set up resize observer to automatically fit terminal when container resizes
   */
  private setupResizeObserver(container: HTMLElement, fitAddon: FitAddon): void {
    const resizeObserver = new ResizeObserver(() => {
      try {
        fitAddon.fit();
      } catch (e) {
        // Ignore errors during resize (can happen during rapid resizing)
      }
    });

    resizeObserver.observe(container);

    // Store observer to clean up later if needed
    // (In practice, the observer will be garbage collected when terminal is destroyed)
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
          background: '#1e1e1e',
          foreground: '#d4d4d4',
          cursor: '#ffffff',
          selectionBackground: 'rgba(255, 255, 255, 0.3)',
        }
      : {
          background: '#ffffff',
          foreground: '#333333',
          cursor: '#000000',
          selectionBackground: 'rgba(0, 0, 0, 0.3)',
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
