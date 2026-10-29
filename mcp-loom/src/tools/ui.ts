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
import { getWorkspacePath, readConfigFile } from "../shared/config.js";
import { sendDaemonRequest } from "../shared/daemon.js";

/**
 * Best-effort daemon liveness/detail probe used by {@link getHeartbeat}.
 *
 * Uses the same Unix-socket IPC every other MCP tool uses
 * (`sendDaemonRequest`), not the retired desktop app's console log. A `Ping`
 * establishes bare liveness (bounded by `sendDaemonRequest`'s own timeout —
 * see `shared/daemon.ts`); a best-effort `DaemonStatus` follow-up enriches
 * the result with the in-flight sweep count when the daemon answers it, but
 * never turns a working `Ping` into a failure if `DaemonStatus` itself
 * errors or times out.
 */
export interface DaemonProbeResult {
  alive: boolean;
  inFlightSweeps: number | null;
}

async function probeDaemon(): Promise<DaemonProbeResult> {
  let alive = false;
  try {
    const pingResponse = (await sendDaemonRequest({ type: "Ping" })) as { type?: string };
    alive = pingResponse?.type === "Pong";
  } catch {
    return { alive: false, inFlightSweeps: null };
  }

  if (!alive) {
    return { alive: false, inFlightSweeps: null };
  }

  let inFlightSweeps: number | null = null;
  try {
    const statusResponse = (await sendDaemonRequest({ type: "DaemonStatus" })) as {
      type?: string;
      payload?: { in_flight?: unknown[] };
    };
    if (statusResponse?.type === "DaemonStatus" && Array.isArray(statusResponse.payload?.in_flight)) {
      inFlightSweeps = statusResponse.payload.in_flight.length;
    }
  } catch {
    // Best-effort only — a live daemon that fails/times out on the richer
    // DaemonStatus call is still reported as alive from the Ping above.
  }

  return { alive, inFlightSweeps };
}

/**
 * Turn the daemon liveness probe into the heartbeat payload.
 *
 * The daemon is the only source of truth: `daemon_healthy` when it answers
 * on its Unix socket, `daemon_unreachable` otherwise.
 */
export function classifyHeartbeat(daemon: DaemonProbeResult): {
  status: "daemon_healthy" | "daemon_unreachable";
  message: string;
  daemon: DaemonProbeResult;
} {
  if (daemon.alive) {
    const sweepNote =
      daemon.inFlightSweeps === null
        ? ""
        : ` (${daemon.inFlightSweeps} sweep${daemon.inFlightSweeps === 1 ? "" : "s"} in flight)`;
    return {
      status: "daemon_healthy",
      message: `loom-daemon is responding on its Unix socket${sweepNote}.`,
      daemon,
    };
  }

  return {
    status: "daemon_unreachable",
    message: "loom-daemon did not respond on its Unix socket.",
    daemon,
  };
}

/**
 * Get heartbeat - reports loom-daemon liveness over its Unix socket.
 */
async function getHeartbeat(): Promise<string> {
  return JSON.stringify(classifyHeartbeat(await probeDaemon()), null, 2);
}

/**
 * Get comprehensive UI state including workspace, terminals, and engine status
 */
async function getUIState(): Promise<string> {
  try {
    const workspacePath = getWorkspacePath();

    // Read the effective config across all tiers (issue #4064) so get_ui_state
    // agrees with the daemon / Python / Bash surfaces about the config, rather
    // than reading only the legacy `.loom/config.json`.
    const config: { version: string; terminals: unknown[]; offlineMode?: boolean } | null =
      await readConfigFile();

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
 * - trigger_start, trigger_force_start, trigger_force_factory_reset, stop_engine: removed; they
 *   relied on a file IPC nothing consumed (the daemon is driven via dispatch_sweep etc.)
 * - trigger_factory_reset: Removed with the tools above
 * - trigger_restart_terminal: Use restart_terminal from terminals module
 * - trigger_run_now: Removed (launch_interval was removed too)
 * - get_random_file: Use .loom/scripts/random-file.sh instead
 */
export const uiTools: Tool[] = [
  {
    name: "get_heartbeat",
    description:
      "Get Loom's heartbeat status. Checks the running loom-daemon over its Unix socket: status 'daemon_healthy' (with an in-flight sweep count) when it responds, 'daemon_unreachable' otherwise. Returns top-level status/message plus a 'daemon' detail object.",
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
    case "get_heartbeat": {
      const heartbeat = await getHeartbeat();
      return [{ type: "text", text: heartbeat }];
    }

    case "get_ui_state": {
      const uiState = await getUIState();
      return [{ type: "text", text: uiState }];
    }

    default:
      throw new Error(`Unknown UI tool: ${name}`);
  }
}
