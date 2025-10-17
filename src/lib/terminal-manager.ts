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
  resizeObserver?: ResizeObserver;
  resizeFrame?: number;
  lastKnownCols?: number;
  lastKnownRows?: number;
  windowResizeHandler?: () => void;
}

/**
 * TerminalManager - Manages xterm.js terminal instances
 *
 * IMPORTANT: This class operates on terminal IDs (stable identifiers like "terminal-1").
 * - terminalId parameters are used for both state management and IPC operations with the daemon
 * - xterm.js instances are keyed by terminal ID and recreated if the daemon restarts
 */
export class TerminalManager {
  private terminals: Map<string, ManagedTerminal> = new Map();

  /**
   * Create a new xterm.js terminal instance and attach it to a persistent container
   * The container is created inside #persistent-xterm-containers and shown/hidden via display style
   */
  createTerminal(terminalId: string, _containerId: string): ManagedTerminal | null {
    // Check if terminal already exists
    const existing = this.terminals.get(terminalId);
    if (existing) {
      console.warn(`Terminal ${terminalId} already exists`);
      return existing;
    }

    // Find or create the persistent container
    const persistentArea = document.getElementById("persistent-xterm-containers");
    if (!persistentArea) {
      console.error("persistent-xterm-containers not found - UI not initialized");
      return null;
    }

    // Create a new persistent container for this terminal
    const container = document.createElement("div");
    container.id = `xterm-container-${terminalId}`;
    container.className = "absolute inset-0"; // Full size, positioned absolutely
    container.style.width = "100%";
    container.style.height = "100%";
    container.style.display = "none"; // Hidden by default
    persistentArea.appendChild(container);

    // Get saved font size or use default
    const fontSize = this.getSavedFontSize();

    // Create xterm.js Terminal instance with fixed size matching tmux
    const terminal = new Terminal({
      cols: 80, // Standard width to match tmux
      rows: 24, // Standard height to match tmux
      cursorBlink: true,
      fontSize,
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

    // Hook up input handler - send user input directly to daemon
    terminal.onData((data) => {
      import("@tauri-apps/api/tauri")
        .then(({ invoke }) => {
          invoke("send_terminal_input", { id: terminalId, data }).catch((e) => {
            console.error(`[terminal-input] Failed to send input for ${terminalId}:`, e);
          });
        })
        .catch((e) => {
          console.error(`[terminal-input] Failed to import tauri API:`, e);
        });

      // Clear needs-input state when user types
      import("./state")
        .then(({ getAppState, TerminalStatus: Status }) => {
          const state = getAppState();
          // Find terminal by id
          const terminal = state.getTerminal(terminalId);
          if (terminal?.status === Status.NeedsInput) {
            state.updateTerminal(terminal.id, { status: Status.Idle });
          }
        })
        .catch((e) => {
          console.error(`[terminal-input] Failed to clear needs-input state:`, e);
        });
    });

    // Hook up bell handler - set needs-input state when terminal beeps
    terminal.onBell(() => {
      import("./state")
        .then(({ getAppState, TerminalStatus: Status }) => {
          const state = getAppState();
          // Find terminal by id
          const terminal = state.getTerminal(terminalId);
          if (terminal) {
            state.updateTerminal(terminal.id, { status: Status.NeedsInput });
          }
        })
        .catch((e) => {
          console.error(`[terminal-bell] Failed to set needs-input state:`, e);
        });
    });

    // Store managed terminal
    const managedTerminal: ManagedTerminal = {
      terminal,
      fitAddon,
      container,
      attached: false,
    };
    this.terminals.set(terminalId, managedTerminal);

    this.setupResizeHandling(terminalId, managedTerminal);

    return managedTerminal;
  }

  /**
   * Get a managed terminal by ID
   */
  getTerminal(terminalId: string): ManagedTerminal | undefined {
    return this.terminals.get(terminalId);
  }

  /**
   * Show a terminal (make it visible in the primary view)
   */
  showTerminal(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    managed.container.style.display = "block";
    this.setupResizeHandling(terminalId, managed);
    this.scheduleResize(terminalId);
    console.log(`[terminal-manager] Showing terminal ${terminalId}`);
  }

  /**
   * Hide a terminal (remove it from primary view but keep state)
   */
  hideTerminal(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      console.warn(`Terminal ${terminalId} not found`);
      return;
    }

    this.teardownResizeHandling(managed);
    managed.container.style.display = "none";
    console.log(`[terminal-manager] Hiding terminal ${terminalId}`);
  }

  /**
   * Hide all terminals
   */
  hideAllTerminals(): void {
    for (const [id] of this.terminals) {
      this.hideTerminal(id);
    }
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
    this.scheduleResize(terminalId);
  }

  /**
   * Fit all terminals (no-op for fixed size terminals)
   * Kept for API compatibility
   */
  fitAllTerminals(): void {
    for (const [id] of this.terminals) {
      this.scheduleResize(id);
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

    this.teardownResizeHandling(managed);
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

  /**
   * Adjust font size for a specific terminal
   */
  adjustFontSize(terminalId: string, delta: number): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      return;
    }

    const currentSize = managed.terminal.options.fontSize || 14;
    const newSize = Math.max(8, Math.min(32, currentSize + delta));
    managed.terminal.options.fontSize = newSize;

    // Save to localStorage
    localStorage.setItem("terminal-font-size", newSize.toString());
    this.scheduleResize(terminalId);
  }

  /**
   * Adjust font size for all terminals
   */
  adjustAllFontSizes(delta: number): void {
    if (this.terminals.size === 0) {
      return;
    }

    // Get current size from first terminal or default
    const firstTerminal = this.terminals.values().next().value as ManagedTerminal | undefined;
    const currentSize = firstTerminal?.terminal.options.fontSize || 14;
    const newSize = Math.max(8, Math.min(32, currentSize + delta));

    // Update all terminals
    for (const [id] of this.terminals) {
      const managed = this.terminals.get(id);
      if (managed) {
        managed.terminal.options.fontSize = newSize;
        this.scheduleResize(id);
      }
    }

    // Save to localStorage
    localStorage.setItem("terminal-font-size", newSize.toString());
  }

  /**
   * Reset font size for all terminals to default
   */
  resetAllFontSizes(): void {
    const defaultSize = 14;

    for (const [id] of this.terminals) {
      const managed = this.terminals.get(id);
      if (managed) {
        managed.terminal.options.fontSize = defaultSize;
        this.scheduleResize(id);
      }
    }

    // Remove from localStorage
    localStorage.removeItem("terminal-font-size");
  }

  /**
   * Get saved font size from localStorage
   */
  getSavedFontSize(): number {
    const saved = localStorage.getItem("terminal-font-size");
    if (saved) {
      const size = parseInt(saved, 10);
      if (!Number.isNaN(size) && size >= 8 && size <= 32) {
        return size;
      }
    }
    return 14; // default
  }

  private setupResizeHandling(terminalId: string, managed: ManagedTerminal): void {
    if (typeof ResizeObserver !== "undefined") {
      if (!managed.resizeObserver) {
        managed.resizeObserver = new ResizeObserver(() => {
          this.scheduleResize(terminalId);
        });
        managed.resizeObserver.observe(managed.container);
      }
      return;
    }

    if (!managed.windowResizeHandler) {
      managed.windowResizeHandler = () => {
        this.scheduleResize(terminalId);
      };
      window.addEventListener("resize", managed.windowResizeHandler);
    }
  }

  private teardownResizeHandling(managed: ManagedTerminal): void {
    if (managed.resizeObserver) {
      managed.resizeObserver.disconnect();
      managed.resizeObserver = undefined;
    }

    if (managed.windowResizeHandler) {
      window.removeEventListener("resize", managed.windowResizeHandler);
      managed.windowResizeHandler = undefined;
    }

    if (managed.resizeFrame !== undefined) {
      cancelAnimationFrame(managed.resizeFrame);
      managed.resizeFrame = undefined;
    }
  }

  private scheduleResize(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      return;
    }

    if (managed.resizeFrame !== undefined) {
      return;
    }

    managed.resizeFrame = requestAnimationFrame(() => {
      managed.resizeFrame = undefined;
      this.applyResize(terminalId);
    });
  }

  private applyResize(terminalId: string): void {
    const managed = this.terminals.get(terminalId);
    if (!managed) {
      return;
    }

    const { container, fitAddon, terminal } = managed;
    if (!container.isConnected) {
      return;
    }

    const rect = container.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) {
      return;
    }

    fitAddon.fit();

    const cols = terminal.cols ?? 0;
    const rows = terminal.rows ?? 0;
    if (cols === 0 || rows === 0) {
      return;
    }

    if (managed.lastKnownCols === cols && managed.lastKnownRows === rows) {
      return;
    }

    managed.lastKnownCols = cols;
    managed.lastKnownRows = rows;

    import("@tauri-apps/api/tauri")
      .then(({ invoke }) =>
        invoke("resize_terminal", { id: terminalId, cols, rows }).catch((error) => {
          console.error(`[terminal-manager] Failed to resize tmux session for ${terminalId}:`, error);
        })
      )
      .catch((error) => {
        console.error(`[terminal-manager] Failed to load tauri API for resize:`, error);
      });
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
