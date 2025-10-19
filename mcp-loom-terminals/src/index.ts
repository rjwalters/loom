#!/usr/bin/env node

import { readFile, stat } from "node:fs/promises";
import { exec } from "node:child_process";
import { Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const execAsync = promisify(exec);

const SOCKET_PATH = process.env.LOOM_SOCKET_PATH || join(homedir(), ".loom", "loom-daemon.sock");
const LOOM_DIR = join(homedir(), ".loom");
const STATE_FILE = join(LOOM_DIR, "state.json");

interface Terminal {
  id: string;
  name: string;
  role?: string;
  working_dir?: string;
  tmux_session: string;
  created_at: number;
}

interface StateFile {
  terminals: Terminal[];
  selectedTerminalId: string | null;
  lastUpdated: string;
}

/**
 * Send a request to the Loom daemon and get the response
 */
async function sendDaemonRequest(request: unknown): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const socket = new Socket();
    let buffer = "";

    socket.on("data", (data) => {
      buffer += data.toString();
    });

    socket.on("end", () => {
      try {
        const response = JSON.parse(buffer);
        resolve(response);
      } catch (error) {
        reject(new Error(`Failed to parse daemon response: ${error}`));
      }
    });

    socket.on("error", (error) => {
      reject(new Error(`Failed to connect to Loom daemon at ${SOCKET_PATH}: ${error.message}`));
    });

    socket.connect(SOCKET_PATH, () => {
      socket.write(JSON.stringify(request));
      socket.write("\n");
    });
  });
}

/**
 * Read the state file to get terminal information
 */
async function readStateFile(): Promise<StateFile | null> {
  try {
    const fileStats = await stat(STATE_FILE);
    if (!fileStats.isFile()) {
      return null;
    }

    const content = await readFile(STATE_FILE, "utf-8");
    return JSON.parse(content) as StateFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * List all active terminals from the daemon
 */
async function listTerminals(): Promise<Terminal[]> {
  try {
    const response = (await sendDaemonRequest({
      type: "ListTerminals",
    })) as { type: string; payload?: { terminals: Terminal[] } };

    if (response.type === "TerminalList" && response.payload?.terminals) {
      return response.payload.terminals;
    }

    return [];
  } catch (_error) {
    // If daemon is not running, fall back to state file
    const state = await readStateFile();
    return state?.terminals || [];
  }
}

/**
 * Get terminal output from the log file
 */
async function getTerminalOutput(terminalId: string, lines = 100): Promise<string> {
  try {
    const logPath = `/tmp/loom-${terminalId}.out`;
    const content = await readFile(logPath, "utf-8");
    const allLines = content.split("\n");

    // Get last N lines (excluding empty trailing line)
    const relevantLines = allLines.slice(-lines - 1, -1).filter(Boolean);

    if (relevantLines.length === 0) {
      return "(empty terminal output)";
    }

    return relevantLines.join("\n");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return `Terminal output file not found for ${terminalId}.\n\nThis usually means:\n- The terminal hasn't been created yet, or\n- The terminal was closed`;
    }
    return `Error reading terminal output: ${error}`;
  }
}

/**
 * Send input to a terminal (future feature)
 */
async function sendTerminalInput(terminalId: string, input: string): Promise<string> {
  try {
    const response = (await sendDaemonRequest({
      type: "SendInput",
      payload: {
        id: terminalId,
        data: input,
      },
    })) as { type: string };

    if (response.type === "Success") {
      return "Input sent successfully";
    }

    return `Unexpected response: ${response.type}`;
  } catch (error) {
    return `Error sending input: ${error}`;
  }
}

/**
 * Check tmux server health and count active loom sessions
 */
async function checkTmuxServerHealth(): Promise<{ serverRunning: boolean; sessionCount: number; sessions: string[]; errorMessage?: string }> {
  try {
    const { stdout } = await execAsync("tmux -L loom list-sessions -F '#{session_name}'");
    const sessions = stdout
      .trim()
      .split("\n")
      .filter((s) => s.startsWith("loom-"));

    return {
      serverRunning: true,
      sessionCount: sessions.length,
      sessions,
    };
  } catch (error: unknown) {
    const err = error as { code?: number; stderr?: string };
    return {
      serverRunning: false,
      sessionCount: 0,
      sessions: [],
      errorMessage: err.stderr || String(error),
    };
  }
}

/**
 * Get tmux server information (PID, socket path, version)
 */
async function getTmuxServerInfo(): Promise<{ serverProcess?: string; socketPath: string; socketExists: boolean; tmuxVersion?: string }> {
  const uid = process.getuid?.() || 0;
  const socketPath = `/private/tmp/tmux-${uid}/loom`;

  try {
    // Check if socket exists
    const socketExists = await stat(socketPath)
      .then(() => true)
      .catch(() => false);

    // Find tmux server process
    let serverProcess: string | undefined;
    try {
      const { stdout } = await execAsync("ps aux | grep 'tmux.*-L loom' | grep -v grep");
      serverProcess = stdout.trim();
    } catch {
      serverProcess = undefined;
    }

    // Get tmux version
    let tmuxVersion: string | undefined;
    try {
      const { stdout } = await execAsync("tmux -V");
      tmuxVersion = stdout.trim();
    } catch {
      tmuxVersion = undefined;
    }

    return {
      serverProcess,
      socketPath,
      socketExists,
      tmuxVersion,
    };
  } catch (error) {
    return {
      socketPath,
      socketExists: false,
      errorMessage: String(error),
    };
  }
}

/**
 * Toggle tmux verbose logging by sending SIGUSR2 to tmux server
 */
async function toggleTmuxVerboseLogging(): Promise<{ success: boolean; message: string; pid?: string }> {
  try {
    // Find tmux server PID
    const { stdout } = await execAsync("pgrep -f 'tmux.*-L loom'");
    const pid = stdout.trim();

    if (!pid) {
      return {
        success: false,
        message: "tmux server not found (no process matching 'tmux.*-L loom')",
      };
    }

    // Send SIGUSR2 to toggle logging
    await execAsync(`kill -SIGUSR2 ${pid}`);

    return {
      success: true,
      message: `Sent SIGUSR2 to tmux server (PID ${pid}) - check for tmux-server-${pid}.log`,
      pid,
    };
  } catch (error) {
    return {
      success: false,
      message: `Error toggling tmux verbose logging: ${error}`,
    };
  }
}

const server = new Server(
  {
    name: "loom-terminals",
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
        name: "list_terminals",
        description:
          "List all active Loom terminal sessions with their IDs, names, roles, and working directories. Use this to discover which terminals are available.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "get_terminal_output",
        description:
          "Get the recent output from a specific terminal. Returns the last N lines of output (default 100). Use this to see what a terminal is currently showing.",
        inputSchema: {
          type: "object",
          properties: {
            terminal_id: {
              type: "string",
              description: "Terminal ID (e.g., 'terminal-1', 'terminal-2')",
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
      {
        name: "get_selected_terminal",
        description:
          "Get information about the currently selected (primary) terminal in Loom. Returns the terminal's ID, name, role, and recent output.",
        inputSchema: {
          type: "object",
          properties: {
            lines: {
              type: "number",
              description: "Number of output lines to include (default: 50)",
              default: 50,
            },
          },
        },
      },
      {
        name: "send_terminal_input",
        description:
          "Send input (commands or text) to a specific terminal. Use this to execute commands in a terminal. Note: Input is sent as literal text, so include '\\n' for Enter key.",
        inputSchema: {
          type: "object",
          properties: {
            terminal_id: {
              type: "string",
              description: "Terminal ID (e.g., 'terminal-1')",
            },
            input: {
              type: "string",
              description: "Text or command to send. Use '\\n' to send Enter, '\\u0003' for Ctrl+C",
            },
          },
          required: ["terminal_id", "input"],
        },
      },
      {
        name: "check_tmux_server_health",
        description:
          "Check if tmux server is running and count active loom sessions. Use this to verify tmux server status and detect crashes.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "get_tmux_server_info",
        description:
          "Get tmux server information including PID, socket path, and version. Use this to diagnose tmux server issues and verify socket paths.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "toggle_tmux_verbose_logging",
        description:
          "Toggle tmux verbose logging by sending SIGUSR2 to the tmux server. Creates tmux-server-{PID}.log file. Use this for deep debugging of tmux issues.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
    ],
  };
});

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    switch (name) {
      case "list_terminals": {
        const terminals = await listTerminals();

        if (terminals.length === 0) {
          return {
            content: [
              {
                type: "text",
                text: "No active terminals found. Either Loom hasn't been started yet, or all terminals have been closed.",
              },
            ],
          };
        }

        const terminalList = terminals
          .map((t) => {
            const parts = [
              `ID: ${t.id}`,
              `Name: ${t.name}`,
              t.role ? `Role: ${t.role}` : null,
              t.working_dir ? `Working Dir: ${t.working_dir}` : null,
              `Session: ${t.tmux_session}`,
            ]
              .filter(Boolean)
              .join("\n  ");
            return `• ${parts}`;
          })
          .join("\n\n");

        return {
          content: [
            {
              type: "text",
              text: `=== Active Loom Terminals (${terminals.length}) ===\n\n${terminalList}`,
            },
          ],
        };
      }

      case "get_terminal_output": {
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

        const output = await getTerminalOutput(terminalId, lines);
        return {
          content: [
            {
              type: "text",
              text: `=== Terminal ${terminalId} Output (last ${lines} lines) ===\n\n${output}`,
            },
          ],
        };
      }

      case "get_selected_terminal": {
        const lines = (args?.lines as number) || 50;
        const state = await readStateFile();

        if (!state || !state.selectedTerminalId) {
          return {
            content: [
              {
                type: "text",
                text: "No terminal is currently selected in Loom.",
              },
            ],
          };
        }

        const terminal = state.terminals.find((t) => t.id === state.selectedTerminalId);

        if (!terminal) {
          return {
            content: [
              {
                type: "text",
                text: `Selected terminal ${state.selectedTerminalId} not found in state file.`,
              },
            ],
          };
        }

        const output = await getTerminalOutput(terminal.id, lines);

        const info = [
          `ID: ${terminal.id}`,
          `Name: ${terminal.name}`,
          terminal.role ? `Role: ${terminal.role}` : null,
          terminal.working_dir ? `Working Dir: ${terminal.working_dir}` : null,
          `Session: ${terminal.tmux_session}`,
        ]
          .filter(Boolean)
          .join("\n");

        return {
          content: [
            {
              type: "text",
              text: `=== Currently Selected Terminal ===\n\n${info}\n\n=== Output (last ${lines} lines) ===\n\n${output}`,
            },
          ],
        };
      }

      case "send_terminal_input": {
        const terminalId = args?.terminal_id as string;
        const input = args?.input as string;

        if (!terminalId || !input) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id and input are required",
              },
            ],
          };
        }

        const result = await sendTerminalInput(terminalId, input);
        return {
          content: [
            {
              type: "text",
              text: result,
            },
          ],
        };
      }

      case "check_tmux_server_health": {
        const health = await checkTmuxServerHealth();

        if (!health.serverRunning) {
          return {
            content: [
              {
                type: "text",
                text: `=== tmux Server Health ===\n\n🚨 Server Status: NOT RUNNING\n\nError: ${health.errorMessage || "Server not responding"}\n\nThis usually means:\n- tmux server crashed\n- No tmux sessions have been created yet\n- Socket path issue\n\nTo start the server, create a new terminal or run:\n  tmux -L loom new-session -d`,
              },
            ],
          };
        }

        const sessionList = health.sessions.map((s) => `  - ${s}`).join("\n");
        return {
          content: [
            {
              type: "text",
              text: `=== tmux Server Health ===\n\n✅ Server Status: RUNNING\nSession Count: ${health.sessionCount}\n\nActive loom sessions:\n${sessionList || "  (none)"}`,
            },
          ],
        };
      }

      case "get_tmux_server_info": {
        const info = await getTmuxServerInfo();

        let statusText = `=== tmux Server Information ===\n\n`;
        statusText += `Socket Path: ${info.socketPath}\n`;
        statusText += `Socket Exists: ${info.socketExists ? "✅ Yes" : "❌ No"}\n\n`;

        if (info.tmuxVersion) {
          statusText += `tmux Version: ${info.tmuxVersion}\n\n`;
        }

        if (info.serverProcess) {
          statusText += `Server Process:\n${info.serverProcess}\n`;
        } else {
          statusText += `Server Process: ❌ Not found (no matching process)\n`;
        }

        return {
          content: [
            {
              type: "text",
              text: statusText,
            },
          ],
        };
      }

      case "toggle_tmux_verbose_logging": {
        const result = await toggleTmuxVerboseLogging();

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Toggle tmux Verbose Logging ===\n\n❌ Failed\n\n${result.message}`,
              },
            ],
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Toggle tmux Verbose Logging ===\n\n✅ Success\n\n${result.message}\n\nNote: Verbose logging writes to tmux-server-${result.pid}.log in the current directory where the tmux server was started.`,
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
  console.error("Loom Terminals MCP server running on stdio");
}

main().catch((error) => {
  console.error("Fatal error in main():", error);
  process.exit(1);
});
