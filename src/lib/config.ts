import { invoke } from '@tauri-apps/api/tauri';
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
 * Load config from .loom/config.json
 * Workspace must be initialized before calling this
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    if (!cachedWorkspacePath) {
      throw new Error('No workspace set - cannot load config');
    }

    const contents = await invoke<string>('read_config', {
      workspacePath: cachedWorkspacePath
    });

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
    if (!cachedWorkspacePath) {
      return;
    }

    const contents = JSON.stringify(config, null, 2);
    await invoke('write_config', {
      workspacePath: cachedWorkspacePath,
      configJson: contents
    });
  } catch (error) {
    console.error('Failed to save config:', error);
  }
}
