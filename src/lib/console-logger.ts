/**
 * Console Logging to File
 *
 * Intercepts console methods (log, error, warn) and writes output to ~/.loom/console.log
 * This allows MCP tools to read console output for debugging purposes.
 */

import { invoke } from "@tauri-apps/api/core";

// Store original console methods before overriding
// biome-ignore lint/suspicious/noConsole: This file intentionally intercepts console methods
const originalConsoleLog = console.log;
// biome-ignore lint/suspicious/noConsole: This file intentionally intercepts console methods
const originalConsoleError = console.error;
// biome-ignore lint/suspicious/noConsole: This file intentionally intercepts console methods
const originalConsoleWarn = console.warn;

/**
 * Write a log entry to ~/.loom/console.log via Tauri IPC
 *
 * @param level - Log level (INFO, ERROR, WARN)
 * @param args - Arguments to log
 */
async function writeToConsoleLog(level: string, ...args: unknown[]) {
  const timestamp = new Date().toISOString();
  const message = args
    .map((arg) => (typeof arg === "object" ? JSON.stringify(arg) : String(arg)))
    .join(" ");
  const logLine = `[${timestamp}] [${level}] ${message}\n`;

  try {
    await invoke("append_to_console_log", { message: logLine });
  } catch (error) {
    // Silent fail - don't want logging errors to break the app
    // Only log to original console if something goes wrong
    originalConsoleError("[console-logger] Failed to write to log file:", error);
  }
}

/**
 * Initialize console logging interception
 *
 * Overrides console.log, console.error, and console.warn to also write to file.
 * Call this once during app initialization.
 */
export function initConsoleLogger(): void {
  // Override console.log
  console.log = (...args: unknown[]) => {
    originalConsoleLog(...args);
    writeToConsoleLog("INFO", ...args);
  };

  // Override console.error
  console.error = (...args: unknown[]) => {
    originalConsoleError(...args);
    writeToConsoleLog("ERROR", ...args);
  };

  // Override console.warn
  console.warn = (...args: unknown[]) => {
    originalConsoleWarn(...args);
    writeToConsoleLog("WARN", ...args);
  };
}
