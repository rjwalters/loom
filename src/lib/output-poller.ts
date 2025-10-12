import { invoke } from "@tauri-apps/api/tauri";
import { getTerminalManager } from "./terminal-manager";

interface TerminalOutput {
  output: string;
  line_count: number;
}

interface PollerState {
  terminalId: string;
  lastLineCount: number;
  polling: boolean;
  intervalId: number | null;
}

export class OutputPoller {
  private pollers: Map<string, PollerState> = new Map();
  private pollInterval: number = 500; // Poll every 500ms

  /**
   * Start polling for a terminal's output
   */
  startPolling(terminalId: string): void {
    // If already polling, do nothing
    if (this.pollers.has(terminalId)) {
      console.warn(`Already polling terminal ${terminalId}`);
      return;
    }

    const state: PollerState = {
      terminalId,
      lastLineCount: -1, // -1 means first poll - get only visible pane
      polling: true,
      intervalId: null,
    };

    // Initial fetch to get current state
    this.pollOnce(state).then(() => {
      // Start interval polling
      const intervalId = window.setInterval(() => {
        if (state.polling) {
          this.pollOnce(state);
        }
      }, this.pollInterval);

      state.intervalId = intervalId;
    });

    this.pollers.set(terminalId, state);
  }

  /**
   * Stop polling for a terminal's output
   */
  stopPolling(terminalId: string): void {
    const state = this.pollers.get(terminalId);
    if (!state) {
      return;
    }

    state.polling = false;

    if (state.intervalId !== null) {
      window.clearInterval(state.intervalId);
      state.intervalId = null;
    }

    this.pollers.delete(terminalId);
  }

  /**
   * Stop all polling
   */
  stopAll(): void {
    for (const [terminalId] of this.pollers) {
      this.stopPolling(terminalId);
    }
  }

  /**
   * Check if currently polling a terminal
   */
  isPolling(terminalId: string): boolean {
    return this.pollers.has(terminalId);
  }

  /**
   * Perform a single poll for output
   */
  private async pollOnce(state: PollerState): Promise<void> {
    try {
      // First poll: get visible pane only (clean state)
      // Subsequent polls: get only new lines (incremental)
      const startLine =
        state.lastLineCount === -1 ? null : state.lastLineCount > 0 ? state.lastLineCount : null;

      const result = await invoke<TerminalOutput>("get_terminal_output", {
        id: state.terminalId,
        startLine,
      });

      // Write output to xterm.js terminal
      if (result.output && result.output.length > 0) {
        const terminalManager = getTerminalManager();

        // On first poll, clear and write to start fresh
        if (state.lastLineCount === -1) {
          terminalManager.clearAndWriteTerminal(state.terminalId, result.output);
        } else {
          // Subsequent polls: append new content
          terminalManager.writeToTerminal(state.terminalId, result.output);
        }
      }

      // Update last line count (after first poll, this will be > 0)
      state.lastLineCount = result.line_count;
    } catch (error) {
      console.error(`Error polling terminal ${state.terminalId}:`, error);
      // Don't stop polling on error - the daemon might be temporarily unavailable
    }
  }

  /**
   * Set the polling interval (in milliseconds)
   */
  setPollInterval(intervalMs: number): void {
    this.pollInterval = intervalMs;

    // Restart all pollers with new interval
    const activeTerminals = Array.from(this.pollers.keys());
    for (const terminalId of activeTerminals) {
      this.stopPolling(terminalId);
      this.startPolling(terminalId);
    }
  }

  /**
   * Get the current polling interval
   */
  getPollInterval(): number {
    return this.pollInterval;
  }

  /**
   * Get count of active pollers
   */
  getPollerCount(): number {
    return this.pollers.size;
  }

  /**
   * Get list of terminals being polled
   */
  getPolledTerminals(): string[] {
    return Array.from(this.pollers.keys());
  }
}

// Singleton instance
let outputPollerInstance: OutputPoller | null = null;

export function getOutputPoller(): OutputPoller {
  if (!outputPollerInstance) {
    outputPollerInstance = new OutputPoller();
  }
  return outputPollerInstance;
}
