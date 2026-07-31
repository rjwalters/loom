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
import { CONSOLE_LOG_PATH, getWorkspacePath, readConfigFile } from "../shared/config.js";
import { sendDaemonRequest } from "../shared/daemon.js";
import { writeMCPCommand } from "../shared/ipc.js";

/**
 * Desktop-app (retired Electron UI) console-log-derived signal — the ONLY
 * signal `getHeartbeat` used before Issue #4794. `status` here is never
 * surfaced verbatim as the top-level heartbeat status anymore (see
 * {@link classifyHeartbeat}) — it is folded into `desktopApp` in the response
 * so a healthy daemon with a stale/absent desktop log never reports
 * top-level `stale`.
 */
export interface DesktopLogStatus {
  status: "healthy" | "active" | "idle" | "stale" | "unknown" | "not_running";
  message: string;
  lastLogTime: string | null;
  logCount: number;
  recentLogs?: string[];
}

/**
 * Read the retired Electron desktop app's console log and classify its
 * freshness. This is a legacy signal (the desktop app has been retired in
 * favor of the Rust `loom-daemon`) — see {@link classifyHeartbeat} for how it
 * is combined with actual daemon liveness.
 */
async function getDesktopLogStatus(): Promise<DesktopLogStatus> {
  try {
    await access(CONSOLE_LOG_PATH);
    const content = await readFile(CONSOLE_LOG_PATH, "utf-8");
    const lines = content.split("\n").filter(Boolean);

    if (lines.length === 0) {
      return {
        status: "unknown",
        message: "Console log is empty - app may not have started yet",
        lastLogTime: null,
        logCount: 0,
      };
    }

    // Get last log entry
    const lastLine = lines[lines.length - 1];
    const timestampMatch = lastLine.match(/\[([^\]]+)\]/);
    const lastLogTime = timestampMatch ? timestampMatch[1] : null;

    // Calculate time since last log
    let timeSinceLastLog = "unknown";
    let status: DesktopLogStatus["status"] = "unknown";
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

    return {
      status,
      message: `Last log entry was ${timeSinceLastLog}`,
      lastLogTime,
      logCount: lines.length,
      recentLogs: lines.slice(-5),
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return {
        status: "not_running",
        message: "Console log file not found - app is not running or console logging is disabled",
        lastLogTime: null,
        logCount: 0,
      };
    }
    throw error;
  }
}

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
 * Combine the daemon liveness probe with the (legacy) desktop-app log signal
 * into the final heartbeat payload (Issue #4794).
 *
 * Before this, `get_heartbeat` derived its ENTIRE status from the retired
 * Electron desktop app's console log, so a perfectly healthy `loom-daemon`
 * with an ancient/absent desktop log (the common case now that the desktop
 * app is retired) was reported as `status: "stale"` / `"not_running"` — the
 * wrong answer for "is the fleet running?". Now:
 *
 * - A live daemon always yields a top-level `daemon_healthy` status,
 *   regardless of desktop-log freshness — the daemon is the source of truth.
 * - A dead/unreachable daemon falls back to the desktop-log-derived status
 *   (preserving the original behavior for hosts that still run the legacy
 *   desktop app, or as a last-resort diagnostic when neither is running).
 * - The raw desktop-log signal is always included under `desktopApp` so
 *   nothing is lost relative to the old response shape.
 */
export function classifyHeartbeat(
  daemon: DaemonProbeResult,
  desktopApp: DesktopLogStatus
): {
  status: string;
  message: string;
  daemon: { alive: boolean; inFlightSweeps: number | null };
  desktopApp: DesktopLogStatus;
  // Legacy top-level fields preserved for backwards compatibility with
  // existing callers that read `lastLogTime` / `logCount` / `recentLogs`
  // directly off the heartbeat response instead of `desktopApp.*`.
  lastLogTime: string | null;
  logCount: number;
  recentLogs?: string[];
} {
  const legacyFields = {
    lastLogTime: desktopApp.lastLogTime,
    logCount: desktopApp.logCount,
    recentLogs: desktopApp.recentLogs,
  };

  if (daemon.alive) {
    const sweepNote =
      daemon.inFlightSweeps === null
        ? ""
        : ` (${daemon.inFlightSweeps} sweep${daemon.inFlightSweeps === 1 ? "" : "s"} in flight)`;
    return {
      status: "daemon_healthy",
      message: `loom-daemon is responding on its Unix socket${sweepNote}. Desktop app console log status: ${desktopApp.status} (${desktopApp.message}).`,
      daemon,
      desktopApp,
      ...legacyFields,
    };
  }

  // Daemon unreachable — fall back to the desktop-log-derived status, but
  // make the daemon's absence explicit so "stale" never gets misread as
  // "the fleet is down" when it may simply mean "no desktop app installed".
  return {
    status: desktopApp.status,
    message: `${desktopApp.message} (loom-daemon did not respond on its Unix socket — this reflects the retired desktop app's log only, not fleet health)`,
    daemon,
    desktopApp,
    ...legacyFields,
  };
}

/**
 * Get app heartbeat - check daemon liveness first (Issue #4794), falling
 * back to the retired desktop app's console log only when the daemon itself
 * is unreachable.
 */
async function getHeartbeat(): Promise<string> {
  const [daemon, desktopApp] = await Promise.all([probeDaemon(), getDesktopLogStatus()]);
  return JSON.stringify(classifyHeartbeat(daemon, desktopApp), null, 2);
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
      "Get Loom's heartbeat status. Checks the running loom-daemon over its Unix socket FIRST (status 'daemon_healthy' whenever the daemon responds, with an in-flight sweep count) and only falls back to the retired Electron desktop app's console log (healthy/active/idle/stale/not_running) when the daemon itself is unreachable. Returns both signals: top-level status/message plus 'daemon' and 'desktopApp' detail objects.",
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
