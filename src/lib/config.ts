import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";
import type { AgentStatus, AppState, ColorTheme, Terminal } from "./state";
import { TerminalStatus } from "./state";

const logger = Logger.forComponent("config");

/**
 * State validation options
 */
interface StateValidationOptions {
  /** Maximum age in milliseconds for input requests (default: 1 hour) */
  maxInputRequestAge?: number;
}

/**
 * Persistent configuration stored in .loom/config.json (committed to git).
 * Contains team-shareable terminal definitions (roles, themes, intervals).
 * This file should be committed to git so all team members share the same terminal setup.
 */
export interface TerminalConfig {
  /** Stable terminal ID (e.g., "terminal-1") - persists across restarts */
  id: string;
  /** User-assigned terminal name */
  name: string;
  /** Optional role type (worker, reviewer, architect, etc.) */
  role?: string;
  /** Role-specific configuration (e.g., system prompt, worker type) */
  roleConfig?: Record<string, unknown>;
  /** Theme ID (e.g., "ocean", "forest") or "default" */
  theme?: string;
  /** Custom color theme configuration */
  customTheme?: ColorTheme;
}

/**
 * Root configuration structure for Loom workspace.
 * Stored in .loom/config.json and committed to version control.
 */
export interface LoomConfig {
  /** Configuration format version - enables safe future migrations */
  version: "2";
  /** Array of terminal configurations */
  terminals: TerminalConfig[];
  /** Offline mode flag - when true, skips Claude Code agent launch and uses simple status echoes */
  offlineMode?: boolean;
}

/**
 * Ephemeral runtime state stored in .loom/state.json (gitignored).
 * Contains machine-specific terminal sessions and daemon state that should NOT be committed.
 * This file is regenerated on each machine based on actual running processes.
 */
export interface TerminalState {
  /** Stable terminal ID (matches corresponding TerminalConfig) */
  id: string;
  /** Current runtime status of the terminal */
  status: TerminalStatus;
  /** Whether this terminal is currently focused in the UI */
  isPrimary: boolean;
  /** Active git worktree path (if terminal is working in a worktree) */
  worktreePath?: string;
  /** Running agent process ID (if an AI agent is active) */
  agentPid?: number;
  /** Agent lifecycle state */
  agentStatus?: AgentStatus;
  /** Unix timestamp (ms) of last autonomous interval execution */
  lastIntervalRun?: number;
  /** Queue of pending input requests from the agent */
  pendingInputRequests?: Array<{
    /** Unique request identifier */
    id: string;
    /** The question or prompt */
    prompt: string;
    /** Unix timestamp (ms) when requested */
    timestamp: number;
  }>;
  /** Total milliseconds spent in busy state (for analytics) */
  busyTime?: number;
  /** Total milliseconds spent in idle state (for analytics) */
  idleTime?: number;
  /** Unix timestamp (ms) of last status change */
  lastStateChange?: number;
}

/**
 * Root state structure for Loom workspace.
 * Stored in .loom/state.json and gitignored (machine-specific).
 */
export interface LoomState {
  /** Running daemon process ID */
  daemonPid?: number;
  /** Counter for terminal numbering (name preserved for compatibility) */
  nextAgentNumber: number;
  /** Array of terminal runtime states */
  terminals: TerminalState[];
}

/**
 * Legacy config format (pre-split into config/state).
 * Contained both config and state in one file.
 * Automatically migrated to new format on first load.
 *
 * @deprecated This format is no longer used but supported for migration
 */
interface LegacyConfig {
  /** Terminal number counter */
  nextAgentNumber: number;
  /** Array of terminals with mixed config and state data */
  agents: Array<Terminal & { configId?: string }>;
}

let cachedWorkspacePath: string | null = null;

/**
 * Sets the current workspace path for config/state operations.
 * Must be called before any loadConfig/saveConfig/loadState/saveState operations.
 *
 * @param workspacePath - Absolute path to the workspace directory
 */
export function setConfigWorkspace(workspacePath: string): void {
  cachedWorkspacePath = workspacePath;
}

/**
 * Migrates legacy config format to new split format.
 * Handles three cases of legacy IDs:
 * 1. Dual-ID system (configId field) - uses existing configId
 * 2. UUID or placeholder IDs - generates stable "terminal-N" ID
 * 3. Already stable IDs - uses as-is
 *
 * @param legacy - The legacy config object containing mixed config/state data
 * @returns Object with separated config and state structures
 */
function migrateLegacyConfig(legacy: LegacyConfig): {
  config: LoomConfig;
  state: LoomState;
} {
  logger.info("Migrating from legacy config format", {
    agentCount: legacy.agents.length,
  });

  const terminals: TerminalConfig[] = [];
  const terminalStates: TerminalState[] = [];

  legacy.agents.forEach((agent, index) => {
    // Determine stable ID
    let id: string;

    // Case 1: Has configId (dual-ID system) - use it
    if ("configId" in agent && agent.configId) {
      id = agent.configId;
      logger.info("Using existing configId", {
        id,
        terminalName: agent.name,
      });
    }
    // Case 2: Has UUID or placeholder - generate stable ID
    else if (
      agent.id &&
      (agent.id.includes("-") || agent.id === "__needs_session__" || agent.id === "__unassigned__")
    ) {
      id = `terminal-${index + 1}`;
      logger.info("Generated stable ID", {
        id,
        terminalName: agent.name,
      });
    }
    // Case 3: Already has stable ID
    else {
      id = agent.id;
    }

    // Split into config (persistent) and state (ephemeral)
    terminals.push({
      id,
      name: agent.name,
      role: agent.role,
      roleConfig: agent.roleConfig,
      theme: agent.theme,
      customTheme: agent.customTheme,
    });

    terminalStates.push({
      id,
      status: agent.status,
      isPrimary: agent.isPrimary,
      worktreePath: agent.worktreePath,
      agentPid: agent.agentPid,
      agentStatus: agent.agentStatus,
      lastIntervalRun: agent.lastIntervalRun,
      pendingInputRequests: agent.pendingInputRequests,
      busyTime: agent.busyTime,
      idleTime: agent.idleTime,
      lastStateChange: agent.lastStateChange,
    });
  });

  return {
    config: { version: "2", terminals },
    state: {
      nextAgentNumber: legacy.nextAgentNumber,
      terminals: terminalStates,
    },
  };
}

/**
 * Migrates config from v1 (no version field or version:"1") to v2 format.
 * v1 configs are identified by missing version field or explicit "1" value.
 *
 * @param raw - The raw v1 config object
 * @returns Migrated v2 config with version field
 */
function migrateFromV1(raw: unknown): LoomConfig {
  const v1 = raw as { terminals?: TerminalConfig[]; offlineMode?: boolean };

  logger.info("Migrating config from v1 to v2", {
    terminalCount: v1.terminals?.length ?? 0,
  });

  return {
    version: "2",
    terminals: v1.terminals ?? [],
    offlineMode: v1.offlineMode,
  };
}

/**
 * Migrates config to the latest version based on explicit version field.
 * Handles version detection and delegates to specific migration functions.
 *
 * @param raw - The raw config object (may be any version)
 * @returns Migrated config at latest version (v2)
 * @throws Error if config version is unsupported (future versions)
 */
function migrateToLatest(raw: unknown): LoomConfig {
  // Safely extract version field
  const version = (raw as { version?: string }).version ?? "1";

  logger.info("Checking config version for migration", { version });

  switch (version) {
    case "1":
      // Treat missing version or explicit "1" as v1
      return migrateFromV1(raw);
    case "2":
      // Already latest version
      return raw as LoomConfig;
    default:
      // Unknown version from the future - fail fast with clear error
      throw new Error(
        `Unsupported config version "${version}". This version of Loom only supports versions 1-2. Please upgrade Loom to work with this config.`
      );
  }
}

/**
 * Loads configuration from .loom/config.json.
 * Automatically migrates legacy format if detected (has "agents" field instead of "terminals").
 * Also migrates from v1 (no version field) to v2 (explicit version).
 * Returns empty config on error.
 *
 * @returns The loaded configuration, or empty config if file doesn't exist or is invalid
 * @throws Never throws - returns empty config on error and logs the error
 */
export async function loadConfig(): Promise<LoomConfig> {
  // Return empty v2 config if no workspace is set
  if (!cachedWorkspacePath) {
    return { version: "2", terminals: [] };
  }

  const workspacePath = cachedWorkspacePath;

  try {
    const contents = await invoke<string>("read_config", {
      workspacePath,
    });

    const parsed = JSON.parse(contents);

    // Check if legacy format (has "agents" array with mixed data)
    if (parsed.agents && !parsed.terminals) {
      logger.info("Detected legacy format, migrating");
      const { config, state } = migrateLegacyConfig(parsed as LegacyConfig);

      // Ensure version field is set after legacy migration
      const configWithVersion: LoomConfig = {
        ...config,
        version: "2",
      };

      // Save migrated versions
      await saveConfig(configWithVersion);
      await saveState(state);

      return configWithVersion;
    }

    // Migrate to latest version if needed
    const migratedConfig = migrateToLatest(parsed);

    // Save migrated config if version changed
    if (!parsed.version || parsed.version !== "2") {
      await saveConfig(migratedConfig);
    }

    return migratedConfig;
  } catch (error) {
    logger.error("Failed to load config", error as Error, { workspacePath });
    // Return empty v2 config on error
    return { version: "2", terminals: [] };
  }
}

/**
 * Saves configuration to .loom/config.json.
 * Creates the .loom directory if it doesn't exist.
 * Formats JSON with 2-space indentation for readability.
 * Always saves with current version ("2") to ensure version field is present.
 *
 * @param config - The configuration to save
 * @throws Never throws - logs error and continues on failure
 */
export async function saveConfig(config: LoomConfig): Promise<void> {
  // Do nothing if no workspace is set
  if (!cachedWorkspacePath) {
    return;
  }

  try {
    const workspacePath = cachedWorkspacePath;

    // Ensure version field is always set to current version
    const configWithVersion: LoomConfig = {
      ...config,
      version: "2",
    };

    const contents = JSON.stringify(configWithVersion, null, 2);
    await invoke("write_config", {
      workspacePath,
      configJson: contents,
    });
  } catch (error) {
    logger.error("Failed to save config", error as Error, { workspacePath: cachedWorkspacePath });
  }
}

/**
 * Loads runtime state from .loom/state.json.
 * Returns default state (nextAgentNumber: 1, empty terminals array) on error.
 *
 * @returns The loaded state, or default state if file doesn't exist or is invalid
 * @throws Never throws - returns default state on error and logs the error
 */
export async function loadState(): Promise<LoomState> {
  // Return default state if no workspace is set
  if (!cachedWorkspacePath) {
    return {
      nextAgentNumber: 1,
      terminals: [],
    };
  }

  try {
    const workspacePath = cachedWorkspacePath;

    const contents = await invoke<string>("read_state", {
      workspacePath,
    });

    return JSON.parse(contents) as LoomState;
  } catch (error) {
    logger.error("Failed to load state", error as Error, { workspacePath: cachedWorkspacePath });
    // Return empty state on error
    return {
      nextAgentNumber: 1,
      terminals: [],
    };
  }
}

/**
 * Saves runtime state to .loom/state.json.
 * Sanitizes runtime-only flags that shouldn't persist across restarts:
 * - Removes missingSession flag (re-evaluated on startup)
 * - Resets error status to idle if it was only due to missing session
 *
 * @param state - The state to save
 * @throws Never throws - logs error and continues on failure
 */
export async function saveState(state: LoomState): Promise<void> {
  // Do nothing if no workspace is set
  if (!cachedWorkspacePath) {
    return;
  }

  try {
    const workspacePath = cachedWorkspacePath;

    // Strip runtime-only flags before persisting
    // missingSession is a runtime status indicator that should be re-evaluated on startup
    const sanitized: LoomState = {
      ...state,
      terminals: state.terminals.map((terminal) => {
        // Remove missingSession if present (defensive - splitTerminals should already exclude it)
        const { missingSession, ...rest } = terminal as TerminalState & {
          missingSession?: boolean;
        };

        // Also reset error status if it was only due to missing session
        if (rest.status === TerminalStatus.Error && missingSession) {
          return { ...rest, status: TerminalStatus.Idle };
        }

        return rest;
      }),
    };

    const contents = JSON.stringify(sanitized, null, 2);
    await invoke("write_state", {
      workspacePath,
      stateJson: contents,
    });
  } catch (error) {
    logger.error("Failed to save state", error as Error, { workspacePath: cachedWorkspacePath });
  }
}

/**
 * Merges configuration and state into full Terminal objects.
 * Combines persistent config (roles, themes) with ephemeral state (status, PIDs).
 * Uses default values for any missing state fields.
 *
 * @param config - The persistent configuration
 * @param state - The ephemeral runtime state
 * @returns Object containing merged terminals array and nextAgentNumber counter
 */
export function mergeConfigAndState(
  config: LoomConfig,
  state: LoomState
): { terminals: Terminal[]; nextAgentNumber: number } {
  const stateMap = new Map(state.terminals.map((s) => [s.id, s]));

  const terminals: Terminal[] = config.terminals.map((cfg) => {
    const st = stateMap.get(cfg.id);

    return {
      id: cfg.id,
      name: cfg.name,
      status: st?.status ?? TerminalStatus.Idle,
      isPrimary: st?.isPrimary ?? false,
      role: cfg.role,
      roleConfig: cfg.roleConfig,
      theme: cfg.theme,
      customTheme: cfg.customTheme,
      worktreePath: st?.worktreePath,
      agentPid: st?.agentPid,
      agentStatus: st?.agentStatus,
      lastIntervalRun: st?.lastIntervalRun,
      pendingInputRequests: st?.pendingInputRequests,
      busyTime: st?.busyTime,
      idleTime: st?.idleTime,
      lastStateChange: st?.lastStateChange,
    };
  });

  return {
    terminals,
    nextAgentNumber: state.nextAgentNumber,
  };
}

/**
 * Splits Terminal objects into separate config and state arrays.
 * Separates persistent configuration (roles, themes) from ephemeral state (status, PIDs).
 * Resets error status to idle before persisting (health monitor will re-detect issues).
 *
 * @param terminals - Array of full Terminal objects to split
 * @returns Object containing separate config and state arrays
 */
export function splitTerminals(terminals: Terminal[]): {
  config: TerminalConfig[];
  state: TerminalState[];
} {
  const config: TerminalConfig[] = terminals.map((t) => ({
    id: t.id,
    name: t.name,
    role: t.role,
    roleConfig: t.roleConfig,
    theme: t.theme,
    customTheme: t.customTheme,
  }));

  const state: TerminalState[] = terminals.map((t) => ({
    id: t.id,
    // Don't persist error status - terminals should start as idle
    // Health monitor will re-detect missing sessions if they actually don't exist
    status: t.status === TerminalStatus.Error ? TerminalStatus.Idle : t.status,
    isPrimary: t.isPrimary,
    worktreePath: t.worktreePath,
    agentPid: t.agentPid,
    agentStatus: t.agentStatus,
    lastIntervalRun: t.lastIntervalRun,
    pendingInputRequests: t.pendingInputRequests,
    busyTime: t.busyTime,
    idleTime: t.idleTime,
    lastStateChange: t.lastStateChange,
  }));

  return { config, state };
}

/**
 * Saves the current configuration and state from AppState.
 * Convenience function that encapsulates the common pattern of:
 * 1. Getting terminals from state
 * 2. Splitting into config and state
 * 3. Saving both files
 *
 * @param state - The AppState instance to save from
 *
 * @example
 * ```ts
 * // Instead of:
 * const terminals = state.getTerminals();
 * const { config, state: terminalStates } = splitTerminals(terminals);
 * await saveConfig({ terminals: config });
 * await saveState({ nextAgentNumber: state.getCurrentTerminalNumber(), terminals: terminalStates });
 *
 * // Use:
 * await saveCurrentConfiguration(state);
 * ```
 */
export async function saveCurrentConfiguration(state: AppState): Promise<void> {
  const terminals = state.getTerminals();
  const { config: terminalConfigs, state: terminalStates } = splitTerminals(terminals);

  await saveConfig({ version: "2", terminals: terminalConfigs });
  await saveState({
    nextAgentNumber: state.getCurrentTerminalNumber(),
    terminals: terminalStates,
  });
}

/**
 * Loads both config and state, merges them, and returns in legacy format.
 * This provides backward compatibility for existing code that expects the
 * { nextAgentNumber, agents } structure instead of separate config/state.
 *
 * @returns Object containing nextAgentNumber counter, merged terminals array (as "agents"), and offline mode flag
 *
 * @example
 * ```ts
 * const { nextAgentNumber, agents, offlineMode } = await loadWorkspaceConfig();
 * state.setNextAgentNumber(nextAgentNumber);
 * state.loadAgents(agents);
 * state.setOfflineMode(offlineMode);
 * ```
 */
export async function loadWorkspaceConfig(): Promise<{
  nextAgentNumber: number;
  agents: Terminal[];
  offlineMode: boolean;
}> {
  const config = await loadConfig();
  const state = await loadState();
  const merged = mergeConfigAndState(config, state);

  return {
    nextAgentNumber: merged.nextAgentNumber,
    agents: merged.terminals,
    offlineMode: config.offlineMode || false,
  };
}

/**
 * Check if a process ID is still alive.
 * Uses the approach of sending signal 0, which doesn't actually send a signal
 * but checks if the process exists.
 *
 * @param pid - Process ID to check
 * @returns True if the process is alive, false otherwise
 */
async function isProcessAlive(pid: number): Promise<boolean> {
  try {
    // Use 'kill -0' which checks if process exists without sending a signal
    const result = await invoke<boolean>("check_process_alive", { pid });
    return result;
  } catch {
    // If the command fails, assume process is dead
    return false;
  }
}

/**
 * Check if a file or directory path exists.
 *
 * @param path - Path to check
 * @returns True if the path exists, false otherwise
 */
async function pathExists(path: string): Promise<boolean> {
  try {
    await invoke("check_path_exists", { path });
    return true;
  } catch {
    return false;
  }
}

/**
 * Validates and cleans up terminal state.
 * Performs the following checks and cleanup:
 * 1. Verifies agent PIDs are still alive
 * 2. Checks worktree paths still exist
 * 3. Removes stale input requests (older than threshold)
 *
 * @param terminals - Array of terminals to validate
 * @param options - Validation options (maxInputRequestAge, etc.)
 * @returns Validated and cleaned terminals array
 *
 * @example
 * ```ts
 * const config = await loadConfig();
 * const state = await loadState();
 * const { terminals } = mergeConfigAndState(config, state);
 *
 * // Validate and clean up state
 * const validatedTerminals = await validateTerminalState(terminals);
 *
 * // Save cleaned state
 * await saveState({
 *   nextAgentNumber: state.nextAgentNumber,
 *   terminals: validatedTerminals.map(t => ({ ...t }))
 * });
 * ```
 */
export async function validateTerminalState(
  terminals: Terminal[],
  options: StateValidationOptions = {}
): Promise<Terminal[]> {
  const { maxInputRequestAge = 60 * 60 * 1000 } = options; // Default: 1 hour
  const now = Date.now();

  logger.info("Validating terminal state", {
    terminalCount: terminals.length,
    maxInputRequestAge,
  });

  const validated = await Promise.all(
    terminals.map(async (terminal) => {
      const updates: Partial<Terminal> = {};
      let needsUpdate = false;

      // 1. Check if agent PID is still alive
      if (terminal.agentPid) {
        const isAlive = await isProcessAlive(terminal.agentPid);
        if (!isAlive) {
          logger.warn("Agent process no longer alive, clearing PID", {
            terminalId: terminal.id,
            terminalName: terminal.name,
            agentPid: terminal.agentPid,
          });
          updates.agentPid = undefined;
          updates.agentStatus = undefined;
          needsUpdate = true;
        }
      }

      // 2. Check if worktree path still exists
      if (terminal.worktreePath) {
        const exists = await pathExists(terminal.worktreePath);
        if (!exists) {
          logger.warn("Worktree path no longer exists, clearing", {
            terminalId: terminal.id,
            terminalName: terminal.name,
            worktreePath: terminal.worktreePath,
          });
          updates.worktreePath = undefined;
          needsUpdate = true;
        }
      }

      // 3. Remove stale input requests
      if (terminal.pendingInputRequests && terminal.pendingInputRequests.length > 0) {
        const freshRequests = terminal.pendingInputRequests.filter((req) => {
          const age = now - req.timestamp;
          const isStale = age > maxInputRequestAge;
          if (isStale) {
            logger.warn("Removing stale input request", {
              terminalId: terminal.id,
              terminalName: terminal.name,
              requestId: req.id,
              ageMs: age,
            });
          }
          return !isStale;
        });

        if (freshRequests.length !== terminal.pendingInputRequests.length) {
          updates.pendingInputRequests = freshRequests;
          needsUpdate = true;
        }
      }

      // Return updated terminal if needed, otherwise return original
      if (needsUpdate) {
        return { ...terminal, ...updates };
      }
      return terminal;
    })
  );

  logger.info("Terminal state validation complete", {
    terminalCount: validated.length,
  });

  return validated;
}
