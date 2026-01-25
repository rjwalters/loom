/**
 * Log tools for Loom MCP server
 *
 * Provides tools for monitoring Loom application logs:
 * - Daemon log (daemon activity, terminal creation, IPC)
 * - Tauri log (frontend activity, state changes, UI errors)
 * - Terminal logs (per-terminal output)
 */

import { readdir, readFile, stat } from "node:fs/promises";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { DAEMON_LOG, TAURI_LOG } from "../shared/config.js";
import { formatLogOutput } from "../shared/formatting.js";
import type { LogResult } from "../types.js";

interface LogTailOptions {
  lines?: number;
}

/**
 * Tail a log file, returning the last N lines
 */
async function tailLogFile(filePath: string, options: LogTailOptions = {}): Promise<LogResult> {
  const { lines = 20 } = options;

  try {
    const fileStats = await stat(filePath);
    if (!fileStats.isFile()) {
      return {
        content: "",
        linesReturned: 0,
        totalLines: 0,
        error: `${filePath} is not a file`,
      };
    }

    const content = await readFile(filePath, "utf-8");
    const allLines = content.split("\n");
    // Filter out empty lines at the end
    const nonEmptyLines = allLines.filter(
      (line, index) => line !== "" || index < allLines.length - 1
    );
    const totalLines = nonEmptyLines.filter(Boolean).length;

    // Get last N lines (excluding empty trailing line)
    const relevantLines = allLines.slice(-lines - 1, -1).filter(Boolean);
    const linesReturned = relevantLines.length;

    return {
      content: relevantLines.join("\n"),
      linesReturned,
      totalLines,
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return {
        content: "",
        linesReturned: 0,
        totalLines: 0,
        error: `Log file not found: ${filePath}\n\nThis usually means:\n- Loom hasn't been started yet, or\n- Logging to file hasn't been configured`,
      };
    }
    return {
      content: "",
      linesReturned: 0,
      totalLines: 0,
      error: `Error reading log: ${error}`,
    };
  }
}

/**
 * List all terminal output logs in /tmp
 */
async function listTerminalLogs(): Promise<string[]> {
  try {
    const tmpFiles = await readdir("/tmp");
    return tmpFiles
      .filter((f) => f.startsWith("loom-") && f.endsWith(".out"))
      .map((f) => `/tmp/${f}`);
  } catch {
    return [];
  }
}

/**
 * Get output from a specific terminal's log file
 */
async function getTerminalLog(terminalId: string, lines: number = 20): Promise<LogResult> {
  const logPath = `/tmp/loom-${terminalId}.out`;
  return tailLogFile(logPath, { lines });
}

/**
 * Log tool definitions
 */
export const logTools: Tool[] = [
  {
    name: "tail_daemon_log",
    description:
      "Tail the Loom daemon log file (~/.loom/daemon.log). Shows recent daemon activity including terminal creation, IPC requests, and errors.",
    inputSchema: {
      type: "object",
      properties: {
        lines: {
          type: "number",
          description: "Number of lines to show (default: 20)",
          default: 20,
        },
      },
    },
  },
  {
    name: "tail_tauri_log",
    description:
      "Tail the Loom Tauri application log file (~/.loom/tauri.log). Shows frontend activity, state changes, and UI errors.",
    inputSchema: {
      type: "object",
      properties: {
        lines: {
          type: "number",
          description: "Number of lines to show (default: 20)",
          default: 20,
        },
      },
    },
  },
  {
    name: "list_terminal_logs",
    description:
      "List all available terminal output logs (/tmp/loom-*.out). Each terminal's output is captured to a separate file.",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },
  {
    name: "tail_terminal_log",
    description:
      "Tail a specific terminal's output log. Terminal IDs are like 'terminal-1', 'terminal-2', etc.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID (e.g., 'terminal-1')",
        },
        lines: {
          type: "number",
          description: "Number of lines to show (default: 20)",
          default: 20,
        },
      },
      required: ["terminal_id"],
    },
  },
];

/**
 * Handle log tool calls
 */
export async function handleLogTool(
  name: string,
  args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  switch (name) {
    case "tail_daemon_log": {
      const lines = (args?.lines as number) || 20;
      const result = await tailLogFile(DAEMON_LOG, { lines });
      return [
        {
          type: "text",
          text: formatLogOutput(result, "Daemon Log"),
        },
      ];
    }

    case "tail_tauri_log": {
      const lines = (args?.lines as number) || 20;
      const result = await tailLogFile(TAURI_LOG, { lines });
      return [
        {
          type: "text",
          text: formatLogOutput(result, "Tauri Log"),
        },
      ];
    }

    case "list_terminal_logs": {
      const logs = await listTerminalLogs();
      if (logs.length === 0) {
        return [
          {
            type: "text",
            text: "No terminal logs found. Terminals may not have been created yet.",
          },
        ];
      }
      return [
        {
          type: "text",
          text: `=== Available Terminal Logs ===\n\n${logs.join("\n")}`,
        },
      ];
    }

    case "tail_terminal_log": {
      const terminalId = args?.terminal_id as string;
      const lines = (args?.lines as number) || 20;

      if (!terminalId) {
        return [
          {
            type: "text",
            text: "Error: terminal_id is required",
          },
        ];
      }

      const result = await getTerminalLog(terminalId, lines);
      return [
        {
          type: "text",
          text: formatLogOutput(result, `Terminal ${terminalId} Log`),
        },
      ];
    }

    default:
      throw new Error(`Unknown log tool: ${name}`);
  }
}
