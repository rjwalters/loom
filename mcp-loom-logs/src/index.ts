#!/usr/bin/env node

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { readdir, readFile, stat } from "fs/promises";
import { homedir } from "os";
import { join } from "path";

const LOOM_DIR = join(homedir(), ".loom");
const DAEMON_LOG = join(LOOM_DIR, "daemon.log");
const TAURI_LOG = join(LOOM_DIR, "tauri.log");

interface LogTailOptions {
  lines?: number;
  follow?: boolean;
}

async function tailLogFile(filePath: string, options: LogTailOptions = {}): Promise<string> {
  const { lines = 100 } = options;

  try {
    const fileStats = await stat(filePath);
    if (!fileStats.isFile()) {
      return `Error: ${filePath} is not a file`;
    }

    const content = await readFile(filePath, "utf-8");
    const allLines = content.split("\n");

    // Get last N lines (excluding empty trailing line)
    const relevantLines = allLines.slice(-lines - 1, -1).filter(Boolean);

    if (relevantLines.length === 0) {
      return `(empty log file)`;
    }

    return relevantLines.join("\n");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return `Log file not found: ${filePath}\n\nThis usually means:\n- Loom hasn't been started yet, or\n- Logging to file hasn't been configured`;
    }
    return `Error reading log: ${error}`;
  }
}

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

async function getTerminalLog(terminalId: string, lines: number = 100): Promise<string> {
  const logPath = `/tmp/loom-${terminalId}.out`;
  return tailLogFile(logPath, { lines });
}

const server = new Server(
  {
    name: "loom-logs",
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

server.setRequestHandler(ListToolsRequestSchema, async () => {
  return {
    tools: [
      {
        name: "tail_daemon_log",
        description:
          "Tail the Loom daemon log file (~/.loom/daemon.log). Shows recent daemon activity including terminal creation, IPC requests, and errors.",
        inputSchema: {
          type: "object",
          properties: {
            lines: {
              type: "number",
              description: "Number of lines to show (default: 100)",
              default: 100,
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
              description: "Number of lines to show (default: 100)",
              default: 100,
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
              description: "Number of lines to show (default: 100)",
              default: 100,
            },
          },
          required: ["terminal_id"],
        },
      },
    ],
  };
});

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    switch (name) {
      case "tail_daemon_log": {
        const lines = (args?.lines as number) || 100;
        const content = await tailLogFile(DAEMON_LOG, { lines });
        return {
          content: [
            {
              type: "text",
              text: `=== Daemon Log (last ${lines} lines) ===\n\n${content}`,
            },
          ],
        };
      }

      case "tail_tauri_log": {
        const lines = (args?.lines as number) || 100;
        const content = await tailLogFile(TAURI_LOG, { lines });
        return {
          content: [
            {
              type: "text",
              text: `=== Tauri Log (last ${lines} lines) ===\n\n${content}`,
            },
          ],
        };
      }

      case "list_terminal_logs": {
        const logs = await listTerminalLogs();
        if (logs.length === 0) {
          return {
            content: [
              {
                type: "text",
                text: "No terminal logs found. Terminals may not have been created yet.",
              },
            ],
          };
        }
        return {
          content: [
            {
              type: "text",
              text: `=== Available Terminal Logs ===\n\n${logs.join("\n")}`,
            },
          ],
        };
      }

      case "tail_terminal_log": {
        const terminalId = args?.terminal_id as string;
        const lines = (args?.lines as number) || 100;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
          };
        }

        const content = await getTerminalLog(terminalId, lines);
        return {
          content: [
            {
              type: "text",
              text: `=== Terminal ${terminalId} Log (last ${lines} lines) ===\n\n${content}`,
            },
          ],
        };
      }

      default:
        return {
          content: [
            {
              type: "text",
              text: `Unknown tool: ${name}`,
            },
          ],
          isError: true,
        };
    }
  } catch (error) {
    return {
      content: [
        {
          type: "text",
          text: `Error: ${error}`,
        },
      ],
      isError: true,
    };
  }
});

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("Loom Logs MCP server running on stdio");
}

main().catch((error) => {
  console.error("Fatal error in main():", error);
  process.exit(1);
});
