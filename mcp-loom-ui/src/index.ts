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
import fg from "fast-glob";
import ignore from "ignore";

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
 * Write MCP command to control file for Loom to pick up with retry and exponential backoff
 * This is a file-based IPC mechanism with acknowledgment
 */
async function writeMCPCommand(command: string): Promise<string> {
  const { writeFile, mkdir, access, readFile, rm } = await import("node:fs/promises");
  const loomDir = join(homedir(), ".loom");
  const commandFile = join(loomDir, "mcp-command.json");
  const ackFile = join(loomDir, "mcp-ack.json");

  // Ensure .loom directory exists
  try {
    await mkdir(loomDir, { recursive: true });
  } catch (_error) {
    // Directory might already exist, that's fine
  }

  // Clean up old acknowledgment file before writing new command
  try {
    await rm(ackFile);
  } catch (_error) {
    // Ack file might not exist, that's fine
  }

  // Write command with timestamp
  const commandData = {
    command,
    timestamp: new Date().toISOString(),
  };

  await writeFile(commandFile, JSON.stringify(commandData, null, 2));

  // Retry with exponential backoff to wait for acknowledgment
  const maxRetries = 8; // Max 8 retries
  const baseDelay = 100; // Start with 100ms
  const maxDelay = 5000; // Cap at 5 seconds
  let totalWaitTime = 0;

  for (let attempt = 0; attempt < maxRetries; attempt++) {
    // Calculate exponential backoff delay: 100ms, 200ms, 400ms, 800ms, 1600ms, 3200ms, 5000ms, 5000ms
    const delay = Math.min(baseDelay * 2 ** attempt, maxDelay);
    totalWaitTime += delay;

    // Wait before checking for acknowledgment
    await new Promise((resolve) => setTimeout(resolve, delay));

    // Check if acknowledgment file exists
    try {
      await access(ackFile);

      // Read acknowledgment data
      const ackContent = await readFile(ackFile, "utf-8");
      const ackData = JSON.parse(ackContent);

      // Verify the ack is for our command
      if (ackData.command === command && ackData.timestamp === commandData.timestamp) {
        // Clean up acknowledgment file
        try {
          await rm(ackFile);
        } catch (_error) {
          // Ignore cleanup errors
        }

        if (ackData.success) {
          return `MCP command '${command}' processed successfully (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        } else {
          return `MCP command '${command}' acknowledged but execution failed (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        }
      }
    } catch (_error) {
      // Ack file doesn't exist yet or couldn't be read, continue retrying
    }
  }

  // Max retries exceeded - give up but don't error
  return `MCP command '${command}' written but no acknowledgment received after ${maxRetries} retries (${totalWaitTime}ms total). The command may still be processing.`;
}

/**
 * Trigger workspace start with existing config (shows confirmation dialog)
 */
async function triggerStart(): Promise<string> {
  return await writeMCPCommand("trigger_start");
}

/**
 * Trigger force start with existing config (no confirmation dialog)
 */
async function triggerForceStart(): Promise<string> {
  return await writeMCPCommand("trigger_force_start");
}

/**
 * Trigger factory reset - overwrites config with defaults (shows confirmation dialog)
 * Note: Factory reset does NOT auto-start. User must run "Start" after reset.
 */
async function triggerFactoryReset(): Promise<string> {
  return await writeMCPCommand("trigger_factory_reset");
}

/**
 * Trigger force factory reset - overwrites config with defaults (NO confirmation dialog)
 * Note: Factory reset does NOT auto-start. User must run "Start" after reset.
 */
async function triggerForceFactoryReset(): Promise<string> {
  return await writeMCPCommand("trigger_force_factory_reset");
}

/**
 * Trigger terminal restart - destroys and recreates a terminal with the same configuration
 */
async function triggerRestartTerminal(terminalId: string): Promise<string> {
  return await writeMCPCommand(`restart_terminal:${terminalId}`);
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

/**
 * Get a random file from the workspace
 * Respects .gitignore and allows custom include/exclude patterns
 */
async function getRandomFile(options?: {
  includePatterns?: string[];
  excludePatterns?: string[];
}): Promise<string> {
  try {
    const workspacePath = process.env.LOOM_WORKSPACE || join(homedir(), "GitHub", "loom");

    // Default exclude patterns
    const defaultExcludes = [
      "**/node_modules/**",
      "**/.git/**",
      "**/dist/**",
      "**/build/**",
      "**/target/**",
      "**/.loom/worktrees/**",
      "**/*.log",
      "**/package-lock.json",
      "**/pnpm-lock.yaml",
      "**/yarn.lock",
    ];

    const excludePatterns = [...defaultExcludes, ...(options?.excludePatterns || [])];
    const includePatterns = options?.includePatterns || ["**/*"];

    // Read .gitignore if it exists
    const gitignorePath = join(workspacePath, ".gitignore");
    let ig = ignore();
    try {
      const gitignoreContent = await readFile(gitignorePath, "utf-8");
      ig = ignore().add(gitignoreContent);
    } catch {
      // .gitignore doesn't exist or can't be read, that's fine
    }

    // Find all files
    const files = await fg(includePatterns, {
      cwd: workspacePath,
      ignore: excludePatterns,
      onlyFiles: true,
      dot: false,
      absolute: false,
    });

    // Filter by .gitignore
    const filteredFiles = files.filter((file) => !ig.ignores(file));

    if (filteredFiles.length === 0) {
      return "No files found matching the criteria";
    }

    // Pick a random file
    const randomIndex = Math.floor(Math.random() * filteredFiles.length);
    const randomFile = filteredFiles[randomIndex];

    // Return absolute path
    return join(workspacePath, randomFile);
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    throw new Error(`Failed to get random file: ${errorMessage}`);
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
          "Start the Loom engine using EXISTING workspace config (.loom/config.json). Shows confirmation dialog before creating terminals and launching agents. Does NOT reset or overwrite config. Use this to restart terminals with current configuration (e.g., after app restart or crash). Requires workspace to be selected.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "trigger_force_start",
        description:
          "Start the Loom engine using existing config WITHOUT confirmation dialog. Same as trigger_start but bypasses confirmation prompt. Use this for MCP automation, testing, or when you're certain the user wants to start. Does NOT reset config. Requires workspace to be selected.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "trigger_factory_reset",
        description:
          "Reset workspace to factory defaults by overwriting .loom/config.json with defaults/config.json. Shows confirmation dialog that requires user interaction. IMPORTANT: This does NOT auto-start the engine - user must separately run trigger_start or trigger_force_start after reset to create terminals. For MCP automation, use trigger_force_factory_reset instead to bypass the confirmation dialog.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "trigger_force_factory_reset",
        description:
          "Reset workspace to factory defaults WITHOUT confirmation dialog. Same as trigger_factory_reset but bypasses confirmation prompt. Use this for MCP automation, testing, or when you're certain the user wants to reset. Does NOT auto-start - must run trigger_force_start after reset to create terminals.",
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
      {
        name: "trigger_restart_terminal",
        description:
          "Restart a specific terminal by destroying and recreating it with the same configuration. The terminal will preserve its name, role, worktree path, and agent configuration. Useful for recovering from stuck terminals or testing terminal lifecycle.",
        inputSchema: {
          type: "object",
          properties: {
            terminalId: {
              type: "string",
              description: "The ID of the terminal to restart (e.g., 'terminal-1')",
            },
          },
          required: ["terminalId"],
        },
      },
      {
        name: "get_random_file",
        description:
          "Get a random file path from the workspace. Respects .gitignore and excludes common build artifacts. Useful for the Critic agent to pick files to review.",
        inputSchema: {
          type: "object",
          properties: {
            includePatterns: {
              type: "array",
              items: { type: "string" },
              description:
                "Optional glob patterns to include (e.g., ['src/**/*.ts']). Defaults to all files.",
            },
            excludePatterns: {
              type: "array",
              items: { type: "string" },
              description:
                "Optional glob patterns to exclude in addition to defaults (node_modules, .git, dist, etc.)",
            },
          },
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

      case "trigger_force_factory_reset": {
        const result = await triggerForceFactoryReset();
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

      case "trigger_restart_terminal": {
        const terminalId = args?.terminalId as string;
        if (!terminalId) {
          throw new Error("terminalId parameter is required");
        }
        const result = await triggerRestartTerminal(terminalId);
        return {
          content: [
            {
              type: "text",
              text: result,
            },
          ],
        };
      }

      case "get_random_file": {
        const includePatterns = args?.includePatterns as string[] | undefined;
        const excludePatterns = args?.excludePatterns as string[] | undefined;
        const randomFile = await getRandomFile({ includePatterns, excludePatterns });
        return {
          content: [
            {
              type: "text",
              text: randomFile,
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
