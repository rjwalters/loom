#!/usr/bin/env node

/**
 * MCP Server for Loom UI Interaction
 *
 * Provides tools for Claude Code to interact with the Loom application:
 * - Read browser console logs (via Tauri log file)
 * - Trigger UI events (factory reset, workspace changes, etc.)
 * - Monitor application state
 */

import { spawn } from "node:child_process";
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
 * Trigger a force start by writing to IPC trigger file
 *
 * Creates a trigger file that the Loom app monitors to emit the
 * force-start-workspace event. This is more reliable than keyboard
 * shortcuts or AppleScript menu access.
 */
async function triggerFactoryReset(): Promise<string> {
  const { writeFile, unlink } = await import("node:fs/promises");
  const triggerFile = join(LOOM_DIR, "force-start.trigger");

  try {
    // Write trigger file
    await writeFile(triggerFile, Date.now().toString(), "utf-8");

    // Wait a bit for the app to process it
    await new Promise((resolve) => setTimeout(resolve, 500));

    // Clean up trigger file
    try {
      await unlink(triggerFile);
    } catch {
      // Ignore cleanup errors
    }

    return "Force start triggered successfully via IPC trigger file";
  } catch (error) {
    throw new Error(
      `Failed to trigger force start: ${error instanceof Error ? error.message : String(error)}`
    );
  }
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
        name: "trigger_factory_reset",
        description:
          "Trigger a force start of the current workspace by writing an IPC trigger file. This resets the workspace to defaults with 6 terminals and launches all agents WITHOUT confirmation dialog. Requires the Loom app to be running.",
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

      case "trigger_factory_reset": {
        const result = await triggerFactoryReset();
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
