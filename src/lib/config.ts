import { invoke } from "@tauri-apps/api/tauri";
import type { Terminal } from "./state";

export interface LoomConfig {
  nextAgentNumber: number;
  agents: Terminal[];
}

let cachedWorkspacePath: string | null = null;

/**
 * Set the current workspace path for config operations
 */
export function setConfigWorkspace(workspacePath: string): void {
  cachedWorkspacePath = workspacePath;
}

/**
 * Load config from .loom/config.json
 * Workspace must be initialized before calling this
 * Returns default config if file doesn't exist
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    if (!cachedWorkspacePath) {
      throw new Error("No workspace set - cannot load config");
    }

    const contents = await invoke<string>("read_config", {
      workspacePath: cachedWorkspacePath,
    });

    const config = JSON.parse(contents) as LoomConfig;
    return config;
  } catch (error) {
    // If config file doesn't exist, return default config
    if (error instanceof Error && error.message.includes("Config file does not exist")) {
      console.log("Config file not found - returning default config");
      return {
        nextAgentNumber: 1,
        agents: [],
      };
    }

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
