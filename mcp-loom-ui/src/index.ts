#!/usr/bin/env node

/**
 * MCP Server for Loom UI Interaction
 *
 * Provides tools for Claude Code to interact with the Loom application:
 * - Read browser console logs (via Tauri log file)
 * - Trigger UI events (factory reset, workspace changes, etc.)
 * - Monitor application state
 */

import { access, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const LOOM_DIR = join(homedir(), ".loom");
const CONSOLE_LOG_PATH = join(LOOM_DIR, "console.log");

/**
 * Read the browser console log file
 */
async function readConsoleLog(lines = 100): Promise<string> {
  try {
    await access(CONSOLE_LOG_PATH);
    const content = await readFile(CONSOLE_LOG_PATH, "utf-8");
    const allLines = content.split("\n");
    const relevantLines = allLines.slice(-lines).filter(Boolean);

    if (relevantLines.length === 0) {
      return "(console log is empty)";
    }

    return relevantLines.join("\n");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return "Console log file not found. The Loom app may need to enable console logging.";
    }
    throw error;
  }
}

/**
 * Write MCP command to control file for Loom to pick up
 * This is a simple file-based IPC mechanism that doesn't require AppleScript
 */
async function writeMCPCommand(command: string): Promise<string> {
  const { writeFile, mkdir } = await import("node:fs/promises");
  const loomDir = join(homedir(), ".loom");
  const commandFile = join(loomDir, "mcp-command.json");

  // Ensure .loom directory exists
  try {
    await mkdir(loomDir, { recursive: true });
  } catch (_error) {
    // Directory might already exist, that's fine
  }

  // Write command with timestamp
  const commandData = {
    command,
    timestamp: new Date().toISOString(),
  };

  await writeFile(commandFile, JSON.stringify(commandData, null, 2));

  return `MCP command '${command}' written to ${commandFile}. Note: File-based IPC is not yet implemented in Loom app. This command will not execute until file watcher is added.`;
}

/**
 * Trigger workspace start (normal reset with confirmation)
 */
async function triggerStart(): Promise<string> {
  return await writeMCPCommand("trigger_start");
}

/**
 * Trigger force start (bypass confirmation)
 */
async function triggerForceStart(): Promise<string> {
  return await writeMCPCommand("trigger_force_start");
}

/**
 * Read the Loom state file
 */
async function readStateFile(): Promise<string> {
  try {
    const workspacePath = process.env.LOOM_WORKSPACE || join(homedir(), "GitHub", "loom");
    const statePath = join(workspacePath, ".loom", "state.json");

    await access(statePath);
    const content = await readFile(statePath, "utf-8");
    return content;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return "State file not found. Workspace may not be initialized.";
    }
    throw error;
  }
}

/**
 * Read the Loom config file
 */
async function readConfigFile(): Promise<string> {
  try {
    const workspacePath = process.env.LOOM_WORKSPACE || join(homedir(), "GitHub", "loom");
    const configPath = join(workspacePath, ".loom", "config.json");

    await access(configPath);
    const content = await readFile(configPath, "utf-8");
    return content;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return "Config file not found. Workspace may not be initialized.";
    }
    throw error;
  }
}

/**
 * Get app heartbeat - check if app is running and logging
 */
async function getHeartbeat(): Promise<string> {
  try {
    await access(CONSOLE_LOG_PATH);
    const content = await readFile(CONSOLE_LOG_PATH, "utf-8");
    const lines = content.split("\n").filter(Boolean);

    if (lines.length === 0) {
      return JSON.stringify(
        {
          status: "unknown",
          message: "Console log is empty - app may not have started yet",
          lastLogTime: null,
          logCount: 0,
        },
        null,
        2
      );
    }

    // Get last log entry
    const lastLine = lines[lines.length - 1];
    const timestampMatch = lastLine.match(/\[([^\]]+)\]/);
    const lastLogTime = timestampMatch ? timestampMatch[1] : null;

    // Calculate time since last log
    let timeSinceLastLog = "unknown";
    let status = "unknown";
    if (lastLogTime) {
      const lastLogDate = new Date(lastLogTime);
      const now = new Date();
      const diffMs = now.getTime() - lastLogDate.getTime();
      const diffSeconds = Math.floor(diffMs / 1000);

      if (diffSeconds < 10) {
        status = "healthy";
        timeSinceLastLog = `${diffSeconds}s ago`;
      } else if (diffSeconds < 60) {
        status = "active";
        timeSinceLastLog = `${diffSeconds}s ago`;
      } else if (diffSeconds < 300) {
        status = "idle";
        const diffMinutes = Math.floor(diffSeconds / 60);
        timeSinceLastLog = `${diffMinutes}m ago`;
      } else {
        status = "stale";
        const diffMinutes = Math.floor(diffSeconds / 60);
        timeSinceLastLog = `${diffMinutes}m ago`;
      }
    }

    return JSON.stringify(
      {
        status,
        message: `Last log entry was ${timeSinceLastLog}`,
        lastLogTime,
        logCount: lines.length,
        recentLogs: lines.slice(-5),
      },
      null,
      2
    );
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return JSON.stringify(
        {
          status: "not_running",
          message: "Console log file not found - app is not running or console logging is disabled",
          lastLogTime: null,
          logCount: 0,
        },
        null,
        2
      );
    }
    throw error;
  }
}

// Create server instance
const server = new Server(
  {
    name: "loom-ui",
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

// List available tools
server.setRequestHandler(ListToolsRequestSchema, async () => {
  return {
    tools: [
      {
        name: "read_console_log",
        description:
          "Read the Loom browser console log to see JavaScript errors, console.log output, and debugging information",
        inputSchema: {
          type: "object",
          properties: {
            lines: {
              type: "number",
              description: "Number of recent lines to return (default: 100)",
              default: 100,
            },
          },
        },
      },
      {
        name: "trigger_start",
        description:
          "Trigger workspace start (factory reset) with confirmation dialog. Shows the user a confirmation modal before resetting workspace to defaults with 6 terminals. Requires the Loom app to be running and a workspace to be selected.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "trigger_force_start",
        description:
          "Trigger force start of the current workspace. This resets the workspace to defaults with 6 terminals and launches all agents WITHOUT confirmation dialog. Use this for automated testing or when you're certain the user wants to reset. Requires the Loom app to be running.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "read_state_file",
        description:
          "Read the current Loom state file (.loom/state.json) to see terminal state, agent numbers, etc.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "read_config_file",
        description:
          "Read the current Loom config file (.loom/config.json) to see terminal configurations and role settings",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "get_heartbeat",
        description:
          "Get app heartbeat status - checks if Loom is running and actively logging. Returns status (healthy/active/idle/stale/not_running), last log time, and recent log entries",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
    ],
  };
});

// Handle tool calls
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    switch (name) {
      case "read_console_log": {
        const lines = (args?.lines as number) || 100;
        const log = await readConsoleLog(lines);
        return {
          content: [
            {
              type: "text",
              text: log,
            },
          ],
        };
      }

      case "trigger_start": {
        const result = await triggerStart();
        return {
          content: [
            {
              type: "text",
              text: result,
            },
          ],
        };
      }

      case "trigger_force_start": {
        const result = await triggerForceStart();
        return {
          content: [
            {
              type: "text",
              text: result,
            },
          ],
        };
      }

      case "read_state_file": {
        const state = await readStateFile();
        return {
          content: [
            {
              type: "text",
              text: state,
            },
          ],
        };
      }

      case "read_config_file": {
        const config = await readConfigFile();
        return {
          content: [
            {
              type: "text",
              text: config,
            },
          ],
        };
      }

      case "get_heartbeat": {
        const heartbeat = await getHeartbeat();
        return {
          content: [
            {
              type: "text",
              text: heartbeat,
            },
          ],
        };
      }

      default:
        throw new Error(`Unknown tool: ${name}`);
    }
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    return {
      content: [
        {
          type: "text",
          text: `Error: ${errorMessage}`,
        },
      ],
      isError: true,
    };
  }
});

// Start server
async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("Loom UI MCP server running on stdio");
}

main().catch((error) => {
  console.error("Server error:", error);
  process.exit(1);
});
