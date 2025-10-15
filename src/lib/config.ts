import { invoke } from "@tauri-apps/api/tauri";
import type { AgentStatus, ColorTheme, Terminal } from "./state";
import { TerminalStatus } from "./state";

/**
 * Persistent configuration stored in .loom/config.json (committed to git)
 * Contains team-shareable terminal definitions (roles, themes, intervals)
 */
export interface TerminalConfig {
  id: string; // Stable terminal ID (e.g., "terminal-1")
  name: string; // User-assigned terminal name
  role?: string; // Role type (worker, reviewer, architect, etc.)
  roleConfig?: Record<string, unknown>; // Role-specific configuration
  theme?: string; // Theme ID or "default"
  customTheme?: ColorTheme; // Custom color theme
}

export interface LoomConfig {
  terminals: TerminalConfig[];
}

/**
 * Ephemeral runtime state stored in .loom/state.json (gitignored)
 * Contains machine-specific terminal sessions and daemon state
 */
export interface TerminalState {
  id: string; // Stable terminal ID (matches config)
  status: TerminalStatus; // Current runtime status
  isPrimary: boolean; // Which terminal is currently focused
  worktreePath?: string; // Active git worktree path
  agentPid?: number; // Running agent process ID
  agentStatus?: AgentStatus; // Agent lifecycle state
  lastIntervalRun?: number; // Last autonomous interval execution (ms)
  pendingInputRequests?: Array<{
    id: string;
    prompt: string;
    timestamp: number;
  }>;
}

export interface LoomState {
  daemonPid?: number; // Running daemon process ID
  nextAgentNumber: number; // Counter for terminal numbering (legacy name for compatibility)
  terminals: TerminalState[];
}

/**
 * Legacy config format (pre-split)
 * Contained both config and state in one file
 */
interface LegacyConfig {
  nextAgentNumber: number;
  agents: Array<Terminal & { configId?: string }>;
}

let cachedWorkspacePath: string | null = null;

/**
 * Set the current workspace path for config/state operations
 */
export function setConfigWorkspace(workspacePath: string): void {
  cachedWorkspacePath = workspacePath;
}

/**
 * Migrate legacy config format to new split format
 * Returns both config and state
 */
function migrateLegacyConfig(legacy: LegacyConfig): {
  config: LoomConfig;
  state: LoomState;
} {
  console.log("[migrateLegacyConfig] Migrating from legacy format...");

  const terminals: TerminalConfig[] = [];
  const terminalStates: TerminalState[] = [];

  legacy.agents.forEach((agent, index) => {
    // Determine stable ID
    let id: string;

    // Case 1: Has configId (dual-ID system) - use it
    if ("configId" in agent && agent.configId) {
      id = agent.configId;
      console.log(`[migrateLegacyConfig] Using configId="${id}" for ${agent.name}`);
    }
    // Case 2: Has UUID or placeholder - generate stable ID
    else if (
      agent.id &&
      (agent.id.includes("-") || agent.id === "__needs_session__" || agent.id === "__unassigned__")
    ) {
      id = `terminal-${index + 1}`;
      console.log(`[migrateLegacyConfig] Generated stable ID "${id}" for ${agent.name}`);
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
    });
  });

  return {
    config: { terminals },
    state: {
      nextAgentNumber: legacy.nextAgentNumber,
      terminals: terminalStates,
    },
  };
}

/**
 * Load config from .loom/config.json
 * Automatically migrates legacy format if needed
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    if (!cachedWorkspacePath) {
      throw new Error("No workspace set - cannot load config");
    }

    const contents = await invoke<string>("read_config", {
      workspacePath: cachedWorkspacePath,
    });

    const parsed = JSON.parse(contents);

    // Check if legacy format (has "agents" array with mixed data)
    if (parsed.agents && !parsed.terminals) {
      console.log("[loadConfig] Detected legacy format, migrating...");
      const { config, state } = migrateLegacyConfig(parsed as LegacyConfig);

      // Save migrated versions
      await saveConfig(config);
      await saveState(state);

      return config;
    }

    return parsed as LoomConfig;
  } catch (error) {
    console.error("Failed to load config:", error);
    // Return empty config on error
    return { terminals: [] };
  }
}

/**
 * Save config to .loom/config.json
 */
export async function saveConfig(config: LoomConfig): Promise<void> {
  try {
    if (!cachedWorkspacePath) {
      return;
    }

    const contents = JSON.stringify(config, null, 2);
    await invoke("write_config", {
      workspacePath: cachedWorkspacePath,
      configJson: contents,
    });
  } catch (error) {
    console.error("Failed to save config:", error);
  }
}

/**
 * Load state from .loom/state.json
 */
export async function loadState(): Promise<LoomState> {
  try {
    if (!cachedWorkspacePath) {
      throw new Error("No workspace set - cannot load state");
    }

    const contents = await invoke<string>("read_state", {
      workspacePath: cachedWorkspacePath,
    });

    return JSON.parse(contents) as LoomState;
  } catch (error) {
    console.error("Failed to load state:", error);
    // Return empty state on error
    return {
      nextAgentNumber: 1,
      terminals: [],
    };
  }
}

/**
 * Save state to .loom/state.json
 */
export async function saveState(state: LoomState): Promise<void> {
  try {
    if (!cachedWorkspacePath) {
      return;
    }

    const contents = JSON.stringify(state, null, 2);
    await invoke("write_state", {
      workspacePath: cachedWorkspacePath,
      stateJson: contents,
    });
  } catch (error) {
    console.error("Failed to save state:", error);
  }
}

/**
 * Merge config and state into full Terminal objects
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
    };
  });

  return {
    terminals,
    nextAgentNumber: state.nextAgentNumber,
  };
}

/**
 * Split Terminal objects into config and state
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
    status: t.status,
    isPrimary: t.isPrimary,
    worktreePath: t.worktreePath,
    agentPid: t.agentPid,
    agentStatus: t.agentStatus,
    lastIntervalRun: t.lastIntervalRun,
    pendingInputRequests: t.pendingInputRequests,
  }));

  return { config, state };
}

/**
 * Load both config and state, merge them, and return in legacy format
 * This provides backward compatibility for existing code that expects
 * { nextAgentNumber, agents } structure
 */
export async function loadWorkspaceConfig(): Promise<{
  nextAgentNumber: number;
  agents: Terminal[];
}> {
  const config = await loadConfig();
  const state = await loadState();
  const merged = mergeConfigAndState(config, state);

  return {
    nextAgentNumber: merged.nextAgentNumber,
    agents: merged.terminals,
  };
}
