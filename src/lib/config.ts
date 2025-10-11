import { writeTextFile, readTextFile, exists, createDir } from '@tauri-apps/api/fs';
import { join } from '@tauri-apps/api/path';
import { Terminal } from './state';

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
 * Get the path to the config file (.loom/config.json)
 */
async function getConfigPath(): Promise<string | null> {
  if (!cachedWorkspacePath) {
    return null;
  }
  const loomDir = await join(cachedWorkspacePath, '.loom');
  return await join(loomDir, 'config.json');
}

/**
 * Load config from .loom/config.json
 * Workspace must be initialized before calling this
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    const configPath = await getConfigPath();
    if (!configPath) {
      throw new Error('No workspace set - cannot load config');
    }

    const fileExists = await exists(configPath);
    if (!fileExists) {
      throw new Error('Config file does not exist - workspace not initialized?');
    }

    const contents = await readTextFile(configPath);
    const config = JSON.parse(contents) as LoomConfig;
    return config;
  } catch (error) {
    console.error('Failed to load config:', error);
    throw error;
  }
}

/**
 * Save config to .loom/config.json
 */
export async function saveConfig(config: LoomConfig): Promise<void> {
  try {
    const configPath = await getConfigPath();
    if (!configPath) {
      return;
    }

    // Ensure .loom directory exists
    const loomDir = await join(cachedWorkspacePath!, '.loom');
    const loomDirExists = await exists(loomDir);
    if (!loomDirExists) {
      await createDir(loomDir, { recursive: true });
    }

    const contents = JSON.stringify(config, null, 2);
    await writeTextFile(configPath, contents);
  } catch (error) {
    console.error('Failed to save config:', error);
  }
}
