/**
 * Shared formatting utilities for Loom MCP server
 *
 * Provides consistent log and output formatting across all tools.
 */

import type { LogResult } from "../types.js";

/**
 * Format terminal output with metadata header
 *
 * Creates a consistent format for displaying terminal output with
 * information about lines returned and total available.
 */
export function formatTerminalOutput(result: LogResult, terminalId: string): string {
  if (result.error) {
    return `--- Terminal ${terminalId} Output (0 lines, file empty or does not exist) ---\n${result.error}`;
  }
  if (result.linesReturned === 0) {
    return `--- Terminal ${terminalId} Output (0 lines, file empty) ---\n(empty terminal output)`;
  }
  return `--- Terminal ${terminalId} Output (${result.linesReturned} lines returned, ${result.totalLines} total lines available) ---\n${result.content}`;
}
