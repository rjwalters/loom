/**
 * Input Logging Layer
 *
 * Logs all terminal input to .loom/logs/input/YYYY-MM-DD.jsonl for analytics
 * and debugging purposes. Designed to be fire-and-forget with no blocking.
 *
 * Entry types:
 * - keystroke: Single character input (< 3 chars)
 * - paste: Multi-character input (>= 10 chars without newline)
 * - enter: Enter key press (contains \r or \n)
 * - command: Input ending with newline (< 10 chars)
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";

const logger = Logger.forComponent("input-logger");

/**
 * Input entry type classification
 */
export type InputType = "keystroke" | "paste" | "enter" | "command";

/**
 * Log entry structure for input events
 */
export interface InputLogEntry {
  /** ISO timestamp of input event */
  timestamp: string;
  /** Type of input: keystroke, paste, enter, or command */
  type: InputType;
  /** Length of input data */
  length: number;
  /** First 50 characters of input for debugging (truncated for privacy) */
  preview: string;
  /** Terminal ID where input occurred */
  terminalId: string;
}

/**
 * InputLogger - Logs terminal input to workspace logs directory
 *
 * Features:
 * - Fire-and-forget logging (never blocks terminal input)
 * - Batched writes using internal buffer
 * - Date-based log files for easy rotation
 * - Input type classification for analytics
 */
export class InputLogger {
  /** Current workspace path */
  private workspacePath: string | null = null;

  /** Buffer for batching log entries */
  private buffer: string[] = [];

  /** Flush timer for batched writes */
  private flushTimer: ReturnType<typeof setTimeout> | null = null;

  /** Flush interval in milliseconds */
  private readonly flushIntervalMs = 1000;

  /** Maximum buffer size before forced flush */
  private readonly maxBufferSize = 50;

  /** Whether the logger is started */
  private started = false;

  /**
   * Start the input logger with a workspace path
   */
  start(workspacePath: string): void {
    this.workspacePath = workspacePath;
    this.started = true;
    logger.info("Input logger started", { workspacePath });
  }

  /**
   * Stop the input logger and flush remaining entries
   */
  async stop(): Promise<void> {
    this.started = false;
    await this.flush();
    this.workspacePath = null;
    logger.info("Input logger stopped");
  }

  /**
   * Log a terminal input event (fire-and-forget)
   *
   * This method never blocks - errors are logged but don't propagate.
   *
   * @param data - The input data from terminal.onData
   * @param terminalId - The terminal ID where input occurred
   */
  log(data: string, terminalId: string): void {
    if (!this.started || !this.workspacePath) {
      return;
    }

    const entry: InputLogEntry = {
      timestamp: new Date().toISOString(),
      type: classifyInputType(data),
      length: data.length,
      preview: data.slice(0, 50).replace(/[\r\n]/g, "\\n"),
      terminalId,
    };

    // Add to buffer as JSONL line
    this.buffer.push(JSON.stringify(entry));

    // Schedule flush if not already scheduled
    if (!this.flushTimer) {
      this.flushTimer = setTimeout(() => {
        this.flush().catch((e) => {
          logger.warn("Failed to flush input log buffer", { error: String(e) });
        });
      }, this.flushIntervalMs);
    }

    // Force flush if buffer is full
    if (this.buffer.length >= this.maxBufferSize) {
      this.flush().catch((e) => {
        logger.warn("Failed to force flush input log buffer", { error: String(e) });
      });
    }
  }

  /**
   * Flush the buffer to disk
   */
  async flush(): Promise<void> {
    // Clear timer
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }

    // Nothing to flush
    if (this.buffer.length === 0 || !this.workspacePath) {
      return;
    }

    // Get entries and clear buffer
    const entries = this.buffer;
    this.buffer = [];

    // Get today's date for log file name
    const logDate = getLogDate();

    try {
      // Write all entries as JSONL (one per line)
      for (const entry of entries) {
        await invoke("append_to_input_log", {
          workspacePath: this.workspacePath,
          logDate,
          entry,
        });
      }
    } catch (error) {
      logger.error("Failed to write input log entries", error as Error, {
        entryCount: entries.length,
      });
    }
  }

  /**
   * Check if the logger is currently active
   */
  isActive(): boolean {
    return this.started && this.workspacePath !== null;
  }

  /**
   * Get the current buffer size (for testing)
   */
  getBufferSize(): number {
    return this.buffer.length;
  }
}

/**
 * Classify input type based on content and length
 */
export function classifyInputType(data: string): InputType {
  // Check for enter/newline first
  if (data === "\r" || data === "\n" || data === "\r\n") {
    return "enter";
  }

  // Check for command (ends with newline but has content)
  if (data.endsWith("\r") || data.endsWith("\n") || data.endsWith("\r\n")) {
    return "command";
  }

  // Check for paste (long input without newline)
  if (data.length >= 10) {
    return "paste";
  }

  // Default to keystroke
  return "keystroke";
}

/**
 * Get today's date in YYYY-MM-DD format for log file naming
 */
export function getLogDate(): string {
  const now = new Date();
  const year = now.getFullYear();
  const month = String(now.getMonth() + 1).padStart(2, "0");
  const day = String(now.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

// Singleton instance
let inputLoggerInstance: InputLogger | null = null;

/**
 * Get the singleton InputLogger instance
 */
export function getInputLogger(): InputLogger {
  if (!inputLoggerInstance) {
    inputLoggerInstance = new InputLogger();
  }
  return inputLoggerInstance;
}

/**
 * Reset the singleton instance (for testing)
 */
export function resetInputLogger(): void {
  if (inputLoggerInstance) {
    inputLoggerInstance.stop().catch(() => {
      // Ignore errors during reset
    });
  }
  inputLoggerInstance = null;
}
