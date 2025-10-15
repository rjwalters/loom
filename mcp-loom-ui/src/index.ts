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
 * Trigger a factory reset via Tauri IPC
 */
async function triggerFactoryReset(): Promise<string> {
  return new Promise((resolve, reject) => {
    // Use osascript to simulate menu click in Loom app
    const script = `
      tell application "System Events"
        tell process "Loom"
          set frontmost to true
          delay 0.2
          click menu item "Factory Reset (Testing)" of menu "Workspace" of menu bar 1
        end tell
      end tell
    `;

    const osascript = spawn("osascript", ["-e", script]);

    let stdout = "";
    let stderr = "";

    osascript.stdout.on("data", (data) => {
      stdout += data.toString();
    });

    osascript.stderr.on("data", (data) => {
      stderr += data.toString();
    });

    osascript.on("close", (code) => {
      if (code === 0) {
        resolve("Factory reset triggered successfully via menu click");
      } else {
        reject(
          new Error(
            `Failed to trigger factory reset (exit code ${code}): ${stderr || "No error message"}`
          )
        );
      }
    });

    // Timeout after 5 seconds
    setTimeout(() => {
      osascript.kill();
      reject(new Error("Factory reset trigger timed out after 5 seconds"));
    }, 5000);
  });
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
          "Trigger a factory reset of the current workspace using macOS accessibility to click the menu item. This resets the workspace to defaults with 6 terminals and launches all agents. Requires the Loom app to be running.",
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
