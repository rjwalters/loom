import { invoke } from "@tauri-apps/api/tauri";
import type { Terminal } from "./state";

export interface LoomConfig {
  nextAgentNumber: number;
  agents: Terminal[];
}

// Old config format (with dual configId/id system or UUID-based ids)
interface LegacyConfig {
  nextAgentNumber: number;
  agents: Array<Terminal & { configId?: string }>;
}

let cachedWorkspacePath: string | null = null;

/**
 * Set the current workspace path for config operations
 */
export function setConfigWorkspace(workspacePath: string): void {
  cachedWorkspacePath = workspacePath;
}

/**
 * Migrate old config format to new single-ID format
 *
 * Handles two migration scenarios:
 * 1. Old dual-ID format: has both configId and id (UUID session ID)
 *    - Use configId, discard the old UUID session ID
 * 2. Very old format: only has id (UUID)
 *    - Generate stable terminal-N IDs
 */
function migrateConfig(config: LegacyConfig | LoomConfig): LoomConfig {
  console.log("[migrateConfig] Checking config format...");

  const migratedAgents: Terminal[] = config.agents.map((agent, index) => {
    // Case 1: Has configId (dual-ID system) - use it and discard UUID
    if ("configId" in agent && agent.configId) {
      console.log(`[migrateConfig] Migrating ${agent.name}: using configId="${agent.configId}"`);
      const { configId, ...rest } = agent;
      return {
        ...rest,
        id: configId, // Use configId as the single ID
      };
    }

    // Case 2: Only has id (old UUID-based system) - generate stable ID
    if (
      agent.id &&
      (agent.id.includes("-") || agent.id === "__needs_session__" || agent.id === "__unassigned__")
    ) {
      const newId = `terminal-${index + 1}`;
      console.log(
        `[migrateConfig] Migrating ${agent.name}: replacing UUID/placeholder with "${newId}"`
      );
      return {
        ...agent,
        id: newId,
      };
    }

    // Case 3: Already has stable ID format (terminal-N) - keep it
    return agent as Terminal;
  });

  return {
    nextAgentNumber: config.nextAgentNumber,
    agents: migratedAgents,
  };
}

/**
 * Load config from .loom/config.json
 * Workspace must be initialized before calling this
 * Automatically migrates old config format to new format
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    if (!cachedWorkspacePath) {
      throw new Error("No workspace set - cannot load config");
    }

    const contents = await invoke<string>("read_config", {
      workspacePath: cachedWorkspacePath,
    });

    const rawConfig = JSON.parse(contents) as LegacyConfig | LoomConfig;
    const config = migrateConfig(rawConfig);

    return config;
  } catch (error) {
    console.error("Failed to load config:", error);
    throw error;
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
