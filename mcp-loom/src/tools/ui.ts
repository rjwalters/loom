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

/** Effective-config shape {@link getUIState} reports on. */
export interface UIStateConfig {
  version: string;
  terminals: unknown[];
  offlineMode?: boolean;
}

/**
 * Assemble the `get_ui_state` payload from already-gathered inputs.
 *
 * Pure (and exported) so the engine-liveness contract below is testable
 * without a workspace on disk or a live daemon.
 *
 * Liveness comes from {@link probeDaemon} — the running `loom-daemon` itself
 * (Issue #13821). It used to be derived from a `.loom/state.json` under the
 * workspace, written only by the Tauri desktop app that #3353 removed. With
 * no writer left, that file never existed, so this tool reported the engine
 * stopped on every call whether or not the daemon was running. The removed
 * `state` / `hasState` blocks echoed the same dead file.
 */
export function buildUIState(
  workspacePath: string,
  config: UIStateConfig | null,
  daemon: DaemonProbeResult
): {
  workspace: { path: string; hasConfig: boolean };
  engine: { isRunning: boolean; inFlightSweeps: number | null };
  config: {
    version: string;
    terminalCount: number;
    offlineMode: boolean;
    terminals: unknown[];
  } | null;
} {
  return {
    workspace: {
      path: workspacePath,
      hasConfig: config !== null,
    },
    engine: {
      isRunning: daemon.alive,
      inFlightSweeps: daemon.inFlightSweeps,
    },
    config: config
      ? {
          version: config.version,
          terminalCount: config.terminals?.length ?? 0,
          offlineMode: config.offlineMode ?? false,
          terminals: config.terminals,
        }
      : null,
  };
}

/**
 * Get comprehensive UI state including workspace, config, and engine liveness
 */
async function getUIState(): Promise<string> {
  try {
    const workspacePath = getWorkspacePath();

    // Read the effective config across all tiers (issue #4064) so get_ui_state
    // agrees with the daemon / Python / Bash surfaces about the config, rather
    // than reading only the legacy `.loom/config.json`.
    const config: UIStateConfig | null = await readConfigFile();

    return JSON.stringify(buildUIState(workspacePath, config, await probeDaemon()), null, 2);
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
 * - read_state_file: Use get_ui_state instead (provides config + more context)
 * - read_config_file: Use get_ui_state instead (provides config + more context)
 * - trigger_start, trigger_force_start, trigger_force_factory_reset, stop_engine: removed; they
 *   relied on a file IPC nothing consumed (the daemon is driven via dispatch_sweep etc.)
 * - trigger_factory_reset: Removed with the tools above
 * - trigger_restart_terminal: Use restart_terminal from terminals module
 * - trigger_run_now: Removed (launch_interval was removed too)
 * - get_random_file: Use .loom/scripts/random-file.sh instead
 * - set_primary_terminal, get_selected_terminal: removed (Issue #13821); they
 *   read and wrote a ~/.loom/state.json the retired desktop app owned, so the
 *   only writer was set_primary_terminal itself. The daemon registry has no
 *   primary/selected terminal — use list_terminals and address a terminal by id.
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
      "Get comprehensive UI state: workspace path, whether a config was resolved, engine liveness (whether loom-daemon answers on its Unix socket, plus its in-flight sweep count), and the effective tiered config including every configured terminal. Use this instead of separate read_state_file and read_config_file calls.",
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
