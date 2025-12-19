import { invoke } from "@tauri-apps/api/core";
import { homeDir } from "@tauri-apps/api/path";
import { Logger } from "./logger";

const logger = Logger.forComponent("history-cache");

/**
 * Configuration for the history cache
 */
export interface HistoryCacheConfig {
  /** Maximum number of lines to keep in memory (default: 10000) */
  maxLines: number;
  /** Maximum size in bytes before truncating (default: 10MB) */
  maxSizeBytes: number;
  /** Debounce interval for disk writes in ms (default: 5000) */
  debounceMs: number;
  /** Whether history caching is enabled (default: true) */
  enabled: boolean;
}

/**
 * Default configuration values
 */
const DEFAULT_CONFIG: HistoryCacheConfig = {
  maxLines: 10000,
  maxSizeBytes: 10 * 1024 * 1024, // 10MB
  debounceMs: 5000, // 5 seconds
  enabled: true,
};

/**
 * Internal buffer state for a terminal
 */
interface BufferState {
  lines: string[];
  dirty: boolean;
  pendingFlush: number | null;
  lastFlushTime: number;
}

/**
 * HistoryCache - Persists terminal output history to disk
 *
 * Features:
 * - Circular buffer to limit memory usage
 * - Debounced writes to reduce disk I/O
 * - Automatic directory creation
 * - Graceful error handling
 *
 * History files are stored in ~/.loom/terminal-history/<terminal-id>.log
 */
export class HistoryCache {
  private config: HistoryCacheConfig;
  private buffers: Map<string, BufferState> = new Map();
  private historyDir: string | null = null;
  private initialized: boolean = false;

  constructor(config: Partial<HistoryCacheConfig> = {}) {
    this.config = { ...DEFAULT_CONFIG, ...config };
  }

  /**
   * Initialize the history cache (creates directory if needed)
   */
  async initialize(): Promise<void> {
    if (this.initialized) {
      return;
    }

    if (!this.config.enabled) {
      logger.info("History cache disabled via configuration");
      this.initialized = true;
      return;
    }

    try {
      const home = await homeDir();
      this.historyDir = `${home}.loom/terminal-history`;
      logger.info("History cache initialized", { historyDir: this.historyDir });
      this.initialized = true;
    } catch (error) {
      logger.error("Failed to initialize history cache", error as Error);
      // Continue without persistence - graceful degradation
      this.config.enabled = false;
      this.initialized = true;
    }
  }

  /**
   * Append output to a terminal's history buffer
   * This is async but non-blocking - callers don't need to await
   */
  async appendOutput(terminalId: string, output: string): Promise<void> {
    if (!this.config.enabled || !this.initialized) {
      return;
    }

    // Get or create buffer state
    let bufferState = this.buffers.get(terminalId);
    if (!bufferState) {
      bufferState = {
        lines: [],
        dirty: false,
        pendingFlush: null,
        lastFlushTime: 0,
      };
      this.buffers.set(terminalId, bufferState);
    }

    // Split output into lines and append
    const newLines = output.split("\n");
    bufferState.lines.push(...newLines);

    // Trim to max lines (circular buffer)
    if (bufferState.lines.length > this.config.maxLines) {
      const excess = bufferState.lines.length - this.config.maxLines;
      bufferState.lines = bufferState.lines.slice(excess);
    }

    bufferState.dirty = true;

    // Schedule debounced flush
    this.scheduleFlush(terminalId, bufferState);
  }

  /**
   * Load cached history for a terminal
   */
  async loadHistory(terminalId: string): Promise<string> {
    if (!this.config.enabled || !this.historyDir) {
      return "";
    }

    const filePath = this.getFilePath(terminalId);

    try {
      const content = await invoke<string>("read_text_file", { path: filePath });
      logger.info("Loaded history from cache", {
        terminalId,
        lineCount: content.split("\n").length,
      });

      // Also populate the in-memory buffer
      const bufferState: BufferState = {
        lines: content.split("\n"),
        dirty: false,
        pendingFlush: null,
        lastFlushTime: Date.now(),
      };
      this.buffers.set(terminalId, bufferState);

      return content;
    } catch (_error) {
      // File doesn't exist or can't be read - this is normal for new terminals
      logger.info("No cached history found", { terminalId });
      return "";
    }
  }

  /**
   * Clear cached history for a terminal
   */
  async clearHistory(terminalId: string): Promise<void> {
    // Clear in-memory buffer
    const bufferState = this.buffers.get(terminalId);
    if (bufferState?.pendingFlush) {
      window.clearTimeout(bufferState.pendingFlush);
    }
    this.buffers.delete(terminalId);

    if (!this.config.enabled || !this.historyDir) {
      return;
    }

    const filePath = this.getFilePath(terminalId);

    try {
      // Write empty content to clear the file (write_file will create if not exists)
      await invoke("write_file", { path: filePath, content: "" });
      logger.info("Cleared history cache", { terminalId });
    } catch (_error) {
      // Ignore errors - file may not exist
      logger.info("No history file to clear", { terminalId });
    }
  }

  /**
   * Flush all pending buffers to disk immediately
   * Call this before app close
   */
  async flushAll(): Promise<void> {
    if (!this.config.enabled) {
      return;
    }

    const flushPromises: Promise<void>[] = [];

    for (const [terminalId, bufferState] of this.buffers) {
      if (bufferState.dirty) {
        // Cancel pending flush
        if (bufferState.pendingFlush) {
          window.clearTimeout(bufferState.pendingFlush);
          bufferState.pendingFlush = null;
        }
        flushPromises.push(this.flushToDisk(terminalId, bufferState));
      }
    }

    await Promise.all(flushPromises);
    logger.info("Flushed all history buffers", { count: flushPromises.length });
  }

  /**
   * Get configuration
   */
  getConfig(): Readonly<HistoryCacheConfig> {
    return { ...this.config };
  }

  /**
   * Update configuration
   */
  updateConfig(config: Partial<HistoryCacheConfig>): void {
    this.config = { ...this.config, ...config };
    logger.info("History cache configuration updated", { config: this.config });
  }

  /**
   * Check if cache is enabled and ready
   */
  isReady(): boolean {
    return this.initialized && this.config.enabled;
  }

  // Private methods

  private scheduleFlush(terminalId: string, bufferState: BufferState): void {
    // If already scheduled, skip
    if (bufferState.pendingFlush) {
      return;
    }

    bufferState.pendingFlush = window.setTimeout(() => {
      bufferState.pendingFlush = null;
      this.flushToDisk(terminalId, bufferState).catch((error) => {
        logger.error("Failed to flush history to disk", error as Error, { terminalId });
      });
    }, this.config.debounceMs);
  }

  private async flushToDisk(terminalId: string, bufferState: BufferState): Promise<void> {
    if (!this.historyDir || !bufferState.dirty) {
      return;
    }

    const filePath = this.getFilePath(terminalId);
    const content = bufferState.lines.join("\n");

    // Check size limit
    const sizeBytes = new Blob([content]).size;
    if (sizeBytes > this.config.maxSizeBytes) {
      logger.warn("History exceeds size limit, truncating", {
        terminalId,
        sizeBytes,
        maxBytes: this.config.maxSizeBytes,
      });
      // Trim to 80% of max size
      const targetLines = Math.floor(bufferState.lines.length * 0.8);
      bufferState.lines = bufferState.lines.slice(-targetLines);
    }

    try {
      await invoke("write_file", { path: filePath, content: bufferState.lines.join("\n") });
      bufferState.dirty = false;
      bufferState.lastFlushTime = Date.now();
      logger.info("Flushed history to disk", {
        terminalId,
        lineCount: bufferState.lines.length,
      });
    } catch (error) {
      logger.error("Failed to write history file", error as Error, { terminalId, filePath });
      // Don't clear dirty flag - will retry on next flush
    }
  }

  private getFilePath(terminalId: string): string {
    // Sanitize terminal ID to be safe for filenames
    const safeId = terminalId.replace(/[^a-zA-Z0-9-_]/g, "_");
    return `${this.historyDir}/${safeId}.log`;
  }
}

// Singleton instance
let historyCacheInstance: HistoryCache | null = null;

/**
 * Get the singleton history cache instance
 */
export function getHistoryCache(): HistoryCache {
  if (!historyCacheInstance) {
    historyCacheInstance = new HistoryCache();
  }
  return historyCacheInstance;
}

/**
 * Initialize the history cache (call this during app startup)
 */
export async function initializeHistoryCache(): Promise<void> {
  const cache = getHistoryCache();
  await cache.initialize();
}
