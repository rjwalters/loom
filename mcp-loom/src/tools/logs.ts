/**
 * Log tools for Loom MCP server
 *
 * REMOVED: Log tools have been removed in favor of using standard shell commands.
 *
 * Use these alternatives instead:
 * - Daemon log: `tail -n 20 ~/.loom/daemon.log`
 * - Tauri log: `tail -n 20 ~/.loom/tauri.log`
 * - Terminal logs: `ls /tmp/loom-*.out` and `tail -n 20 /tmp/loom-terminal-1.out`
 *
 * The MCP server no longer exposes log reading tools to reduce tool count
 * and because agents can use Bash to read logs directly.
 */

import type { Tool } from "@modelcontextprotocol/sdk/types.js";

/**
 * Log tool definitions - empty, all removed
 */
export const logTools: Tool[] = [];

/**
 * Handle log tool calls - no tools to handle
 */
export async function handleLogTool(
  name: string,
  _args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  throw new Error(
    `Unknown log tool: ${name}. Log tools have been removed - use Bash to tail log files directly.`
  );
}
