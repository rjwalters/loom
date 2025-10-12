import { invoke } from "@tauri-apps/api/tauri";
import { getTerminalManager } from "./terminal-manager";

interface TerminalOutput {
  output: string;
  byte_count: number;
}

interface PollerState {
  terminalId: string;
  lastByteCount: number;
  polling: boolean;
  intervalId: number | null;
}

export class OutputPoller {
  private pollers: Map<string, PollerState> = new Map();
  private pollInterval: number = 50; // Poll every 50ms for responsive feel

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
      lastByteCount: 0, // Start from beginning of file
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
      // Get new bytes since last poll (or all bytes on first poll)
      const startByte = state.lastByteCount > 0 ? state.lastByteCount : null;

      console.log(
        `[poller] Polling terminal ${state.terminalId}, startByte=${startByte}, lastByteCount=${state.lastByteCount}`
      );

      const result = await invoke<TerminalOutput>("get_terminal_output", {
        id: state.terminalId,
        startByte,
      });

      console.log(
        `[poller] Received: output.length=${result.output.length}, byte_count=${result.byte_count}`
      );

      // Decode base64 output and write to xterm.js terminal
      if (result.output && result.output.length > 0) {
        // Decode base64 to raw bytes
        const decodedBytes = this.base64ToBytes(result.output);
        console.log(`[poller] Decoded ${decodedBytes.length} bytes from base64`);

        // Convert bytes to string (UTF-8)
        const text = new TextDecoder("utf-8").decode(decodedBytes);
        console.log(
          `[poller] Decoded text length: ${text.length}, preview: ${text.substring(0, 100).replace(/\n/g, "\\n").replace(/\r/g, "\\r")}`
        );

        const terminalManager = getTerminalManager();

        // First poll: clear and write fresh state
        if (state.lastByteCount === 0) {
          console.log(`[poller] First poll - clearing and writing to terminal ${state.terminalId}`);
          terminalManager.clearAndWriteTerminal(state.terminalId, text);
        } else {
          // Subsequent polls: append new content incrementally
          console.log(`[poller] Incremental update - writing to terminal ${state.terminalId}`);
          terminalManager.writeToTerminal(state.terminalId, text);
        }
      } else {
        console.log(`[poller] No output to write (empty or null)`);
      }

      // Update byte offset for next poll
      state.lastByteCount = result.byte_count;
    } catch (error) {
      console.error(`Error polling terminal ${state.terminalId}:`, error);
      // Don't stop polling on error - the daemon might be temporarily unavailable
    }
  }

  /**
   * Decode base64 string to Uint8Array
   */
  private base64ToBytes(base64: string): Uint8Array {
    const binaryString = atob(base64);
    const bytes = new Uint8Array(binaryString.length);
    for (let i = 0; i < binaryString.length; i++) {
      bytes[i] = binaryString.charCodeAt(i);
    }
    return bytes;
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
