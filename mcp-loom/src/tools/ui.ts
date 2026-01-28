/**
 * UI tools for Loom MCP server
 *
 * Provides tools for interacting with the Loom application:
 * - Engine start/stop
 * - Factory reset
 * - Heartbeat monitoring
 * - Comprehensive UI state
 *
 * Note: For random file selection, use .loom/scripts/random-file.sh instead.
 * For reading state/config files, use get_ui_state which provides comprehensive info.
 */

import { access, readFile } from "node:fs/promises";
import { join } from "node:path";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { CONSOLE_LOG_PATH, getWorkspacePath } from "../shared/config.js";
import { writeMCPCommand } from "../shared/ipc.js";

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
 * Get comprehensive UI state including workspace, terminals, and engine status
 */
async function getUIState(): Promise<string> {
  try {
    const workspacePath = getWorkspacePath();

    // Read config file
    const configPath = join(workspacePath, ".loom", "config.json");
    let config: { version: string; terminals: unknown[]; offlineMode?: boolean } | null = null;
    try {
      await access(configPath);
      const configContent = await readFile(configPath, "utf-8");
      config = JSON.parse(configContent);
    } catch {
      // Config doesn't exist or can't be read
    }

    // Read state file
    const statePath = join(workspacePath, ".loom", "state.json");
    let state: {
      daemonPid?: number;
      nextAgentNumber: number;
      terminals: Array<{
        id: string;
        status: string;
        isPrimary: boolean;
        worktreePath?: string;
        agentPid?: number;
        agentStatus?: string;
        lastIntervalRun?: number;
      }>;
    } | null = null;
    try {
      await access(statePath);
      const stateContent = await readFile(statePath, "utf-8");
      state = JSON.parse(stateContent);
    } catch {
      // State doesn't exist or can't be read
    }

    // Build comprehensive UI state response
    const uiState = {
      workspace: {
        path: workspacePath,
        hasConfig: config !== null,
        hasState: state !== null,
      },
      engine: {
        isRunning: state !== null && (state.terminals?.length ?? 0) > 0,
        daemonPid: state?.daemonPid ?? null,
        terminalCount: state?.terminals?.length ?? 0,
      },
      config: config
        ? {
            version: config.version,
            terminalCount: config.terminals?.length ?? 0,
            offlineMode: config.offlineMode ?? false,
            terminals: config.terminals,
          }
        : null,
      state: state
        ? {
            nextAgentNumber: state.nextAgentNumber,
            terminals: state.terminals?.map((t) => ({
              id: t.id,
              status: t.status,
              isPrimary: t.isPrimary,
              worktreePath: t.worktreePath,
              agentPid: t.agentPid,
              agentStatus: t.agentStatus,
              lastIntervalRun: t.lastIntervalRun ? new Date(t.lastIntervalRun).toISOString() : null,
            })),
          }
        : null,
    };

    return JSON.stringify(uiState, null, 2);
  } catch (error) {
    return JSON.stringify(
      {
        error: `Failed to get UI state: ${error}`,
      },
      null,
      2
    );
  }
}

/**
 * UI tool definitions
 *
 * Removed tools (use alternatives):
 * - read_console_log: Use `tail ~/.loom/console.log` for debugging
 * - read_state_file: Use get_ui_state instead (provides state + more context)
 * - read_config_file: Use get_ui_state instead (provides config + more context)
 * - trigger_factory_reset: Use trigger_force_factory_reset (bypasses confirmation)
 * - trigger_restart_terminal: Use restart_terminal from terminals module
 * - trigger_run_now: Use launch_interval from terminals module
 * - get_random_file: Use .loom/scripts/random-file.sh instead
 */
export const uiTools: Tool[] = [
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
    name: "trigger_force_factory_reset",
    description:
      "Reset workspace to factory defaults WITHOUT confirmation dialog. Overwrites .loom/config.json with defaults/config.json. Does NOT auto-start - must run trigger_force_start after reset to create terminals. Use this for MCP automation or when you're certain the user wants to reset.",
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
    name: "stop_engine",
    description:
      "Stop the Loom engine by destroying all terminal sessions and cleaning up resources. This will close all terminals and stop all running agents. Use trigger_start or trigger_force_start to restart the engine afterwards.",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },
  {
    name: "get_ui_state",
    description:
      "Get comprehensive UI state including workspace info, engine status, terminal configurations, and runtime state. Returns a JSON object with workspace path, engine running status, terminal count, and detailed terminal states. Also includes full config and state file contents. Use this instead of separate read_state_file and read_config_file calls.",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },
];

/**
 * Handle UI tool calls
 */
export async function handleUITool(
  name: string,
  _args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  switch (name) {
    case "trigger_start": {
      const result = await writeMCPCommand("trigger_start");
      return [{ type: "text", text: result }];
    }

    case "trigger_force_start": {
      const result = await writeMCPCommand("trigger_force_start");
      return [{ type: "text", text: result }];
    }

    case "trigger_force_factory_reset": {
      const result = await writeMCPCommand("trigger_force_factory_reset");
      return [{ type: "text", text: result }];
    }

    case "get_heartbeat": {
      const heartbeat = await getHeartbeat();
      return [{ type: "text", text: heartbeat }];
    }

    case "stop_engine": {
      const result = await writeMCPCommand("stop_engine");
      return [{ type: "text", text: result }];
    }

    case "get_ui_state": {
      const uiState = await getUIState();
      return [{ type: "text", text: uiState }];
    }

    default:
      throw new Error(`Unknown UI tool: ${name}`);
  }
}
