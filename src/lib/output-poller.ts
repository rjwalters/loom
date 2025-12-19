import { invoke } from "@tauri-apps/api/core";
import { getHistoryCache } from "./history-cache";
import { Logger } from "./logger";
import { getTerminalManager } from "./terminal-manager";

const logger = Logger.forComponent("output-poller");

interface TerminalOutput {
  output: string;
  byte_count: number;
}

interface PollerState {
  terminalId: string;
  lastByteCount: number;
  polling: boolean;
  intervalId: number | null;
  consecutiveErrors: number;
  lastErrorTime: number | null;
  lastOutputTime: number | null; // Timestamp of last output received
  currentPollInterval: number; // Current polling interval (adaptive)
}

/**
 * OutputPoller - Polls daemon for terminal output and writes to xterm.js
 *
 * IMPORTANT: This class operates on terminal IDs (stable identifiers like "terminal-1").
 * - terminalId parameters are used for both state management and IPC operations
 * - Error callback receives terminal ID - caller can use state.terminals.getTerminal(id) to look it up
 * - Pollers are keyed by terminal ID and must be restarted if daemon restarts
 */
export class OutputPoller {
  private pollers: Map<string, PollerState> = new Map();
  private pollInterval: number = 50; // Poll every 50ms for responsive feel (active terminals)
  private idlePollInterval: number = 10000; // Poll every 10s for idle terminals
  private activityTimeout: number = 30000; // Consider idle after 30s of no output
  private maxConsecutiveErrors: number = 5; // Stop polling after this many consecutive errors
  private errorCallback?: (terminalId: string, error: string) => void;
  private activityCallback?: (terminalId: string) => void; // Called when output is received

  /**
   * Start polling for a terminal's output
   */
  startPolling(terminalId: string): void {
    // If already polling, do nothing
    if (this.pollers.has(terminalId)) {
      logger.warn("Already polling terminal", { terminalId });
      return;
    }

    const state: PollerState = {
      terminalId,
      lastByteCount: 0, // Start from beginning of file
      polling: true,
      intervalId: null,
      consecutiveErrors: 0,
      lastErrorTime: null,
      lastOutputTime: null,
      currentPollInterval: this.pollInterval, // Start with active polling
    };

    // Initial fetch to get current state
    this.pollOnce(state).then(() => {
      // Start interval polling with adaptive frequency
      this.scheduleNextPoll(state);
    });

    this.pollers.set(terminalId, state);
  }

  /**
   * Pause polling for a terminal's output (keeps state for resume)
   */
  pausePolling(terminalId: string): void {
    const state = this.pollers.get(terminalId);
    if (!state) {
      return;
    }

    state.polling = false;

    if (state.intervalId !== null) {
      window.clearInterval(state.intervalId);
      state.intervalId = null;
    }

    // Don't delete from map - keep state for resume
  }

  /**
   * Resume polling for a terminal (continues from last byte count)
   */
  resumePolling(terminalId: string): void {
    const state = this.pollers.get(terminalId);
    if (!state) {
      // If no state exists, start fresh
      this.startPolling(terminalId);
      return;
    }

    // Already polling?
    if (state.polling && state.intervalId !== null) {
      logger.warn("Terminal is already actively polling", { terminalId });
      return;
    }

    // Resume polling with existing state
    state.polling = true;

    // Do immediate poll then start interval
    this.pollOnce(state).then(() => {
      this.scheduleNextPoll(state);
    });
  }

  /**
   * Stop polling for a terminal's output (clears state)
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

      const result = await invoke<TerminalOutput>("get_terminal_output", {
        id: state.terminalId,
        startByte,
      });

      // Decode base64 output and write to xterm.js terminal
      if (result.output && result.output.length > 0) {
        // Decode base64 to raw bytes
        const decodedBytes = this.base64ToBytes(result.output);
        logger.info("Decoded output from base64", {
          terminalId: state.terminalId,
          byteCount: decodedBytes.length,
        });

        // Convert bytes to string (UTF-8)
        const text = new TextDecoder("utf-8").decode(decodedBytes);
        const preview = text.substring(0, 100).replace(/\n/g, "\\n").replace(/\r/g, "\\r");
        logger.info("Decoded text", {
          terminalId: state.terminalId,
          textLength: text.length,
          preview,
        });

        const terminalManager = getTerminalManager();

        // First poll: clear and write fresh state
        if (state.lastByteCount === 0) {
          logger.info("First poll - clearing and writing to terminal", {
            terminalId: state.terminalId,
          });
          terminalManager.clearAndWriteTerminal(state.terminalId, text);
        } else {
          // Subsequent polls: append new content incrementally
          logger.info("Incremental update - writing to terminal", {
            terminalId: state.terminalId,
          });
          terminalManager.writeToTerminal(state.terminalId, text);
        }

        // Record activity
        state.lastOutputTime = Date.now();

        // Notify activity callback if registered
        if (this.activityCallback) {
          this.activityCallback(state.terminalId);
        }

        // Cache output to disk for persistence across restarts (async, non-blocking)
        const historyCache = getHistoryCache();
        if (historyCache.isReady()) {
          historyCache.appendOutput(state.terminalId, text).catch((err) => {
            logger.warn("Failed to cache history", {
              terminalId: state.terminalId,
              error: String(err),
            });
          });
        }
      }
      // Silently ignore empty polls - this is normal and expected

      // Update byte offset for next poll
      state.lastByteCount = result.byte_count;

      // Reset error counter on successful poll
      state.consecutiveErrors = 0;
      state.lastErrorTime = null;

      // Adjust polling frequency based on activity
      this.adjustPollingFrequency(state);
    } catch (error) {
      // Track consecutive errors
      state.consecutiveErrors++;
      state.lastErrorTime = Date.now();

      const errorMessage =
        typeof error === "string" ? error : (error as Error)?.message || "Unknown error";

      // Only log errors occasionally to avoid spam (every 5th error, or first error)
      if (state.consecutiveErrors === 1 || state.consecutiveErrors % 5 === 0) {
        logger.error("Error polling terminal", error, {
          terminalId: state.terminalId,
          consecutiveErrors: state.consecutiveErrors,
        });
      }

      // Stop polling after max consecutive errors
      if (state.consecutiveErrors >= this.maxConsecutiveErrors) {
        logger.error("Stopping polling after max consecutive errors", {
          terminalId: state.terminalId,
          maxErrors: this.maxConsecutiveErrors,
        });

        // Notify error callback if registered
        if (this.errorCallback) {
          this.errorCallback(state.terminalId, errorMessage);
        }

        // Stop polling this terminal
        this.stopPolling(state.terminalId);
      }
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

  /**
   * Schedule the next poll for a terminal based on its current state
   */
  private scheduleNextPoll(state: PollerState): void {
    if (!state.polling) {
      return;
    }

    // Clear any existing timer
    if (state.intervalId !== null) {
      window.clearTimeout(state.intervalId);
    }

    // Schedule next poll with current interval
    state.intervalId = window.setTimeout(() => {
      if (state.polling) {
        this.pollOnce(state).then(() => {
          this.scheduleNextPoll(state);
        });
      }
    }, state.currentPollInterval);
  }

  /**
   * Adjust polling frequency based on terminal activity
   */
  private adjustPollingFrequency(state: PollerState): void {
    const now = Date.now();
    const timeSinceActivity = state.lastOutputTime ? now - state.lastOutputTime : Infinity;

    // If terminal has been inactive for activityTimeout, slow down polling
    if (timeSinceActivity > this.activityTimeout) {
      if (state.currentPollInterval !== this.idlePollInterval) {
        logger.info("Terminal idle - reducing poll frequency", {
          terminalId: state.terminalId,
          idleSeconds: Math.round(timeSinceActivity / 1000),
          newInterval: this.idlePollInterval,
        });
        state.currentPollInterval = this.idlePollInterval;
      }
    } else {
      // Terminal is active, use fast polling
      if (state.currentPollInterval !== this.pollInterval) {
        logger.info("Terminal active - increasing poll frequency", {
          terminalId: state.terminalId,
          newInterval: this.pollInterval,
        });
        state.currentPollInterval = this.pollInterval;
      }
    }
  }

  /**
   * Register a callback to be called when output is received (activity detected)
   */
  onActivity(callback: (terminalId: string) => void): void {
    this.activityCallback = callback;
  }

  /**
   * Register a callback to be called when a terminal encounters fatal errors
   */
  onError(callback: (terminalId: string, error: string) => void): void {
    this.errorCallback = callback;
  }

  /**
   * Get error state for a terminal
   */
  getErrorState(
    terminalId: string
  ): { consecutiveErrors: number; lastErrorTime: number | null } | null {
    const state = this.pollers.get(terminalId);
    if (!state) {
      return null;
    }
    return {
      consecutiveErrors: state.consecutiveErrors,
      lastErrorTime: state.lastErrorTime,
    };
  }

  /**
   * Set the maximum number of consecutive errors before stopping polling
   */
  setMaxConsecutiveErrors(max: number): void {
    this.maxConsecutiveErrors = max;
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
