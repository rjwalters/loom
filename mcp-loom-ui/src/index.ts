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
 * Trigger a Tauri menu event
 */
async function triggerMenuEvent(eventName: string): Promise<string> {
  return new Promise((resolve, reject) => {
    // Use osascript to trigger menu items via macOS accessibility
    // This requires the Loom app to be running and focused

    const script = `
      tell application "System Events"
        tell process "loom"
          set frontmost to true
          -- Simulate menu navigation based on event name
          -- This is a placeholder - actual implementation would need to map
          -- event names to specific menu paths
        end tell
      end tell
    `;

    // For now, just return a message indicating the limitation
    resolve(
      `Menu event triggering not yet implemented. Event: ${eventName}\n\nTo implement this, we need to either:\n1. Add Tauri IPC endpoints for triggering events\n2. Use macOS accessibility API via osascript\n3. Use a Chrome DevTools Protocol connection`
    );
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
          "Trigger a factory reset of the current workspace (WARNING: This is a placeholder and doesn't work yet)",
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
        const result = await triggerMenuEvent("factory-reset-workspace");
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
