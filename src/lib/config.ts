import { writeTextFile, readTextFile, exists, createDir } from '@tauri-apps/api/fs';
import { join } from '@tauri-apps/api/path';
import { Terminal, TerminalStatus } from './state';

export interface LoomConfig {
  nextAgentNumber: number;
  agents: Terminal[];
}

const DEFAULT_CONFIG: LoomConfig = {
  nextAgentNumber: 4,
  agents: [
    {
      id: '1',
      name: 'Agent 1',
      status: TerminalStatus.Idle,
      isPrimary: true
    },
    {
      id: '2',
      name: 'Agent 2',
      status: TerminalStatus.Idle,
      isPrimary: false
    },
    {
      id: '3',
      name: 'Agent 3',
      status: TerminalStatus.Idle,
      isPrimary: false
    }
  ]
};

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
 * Returns default config if file doesn't exist
 */
export async function loadConfig(): Promise<LoomConfig> {
  try {
    const configPath = await getConfigPath();
    if (!configPath) {
      console.log('⚙️  No workspace set, using default config');
      return { ...DEFAULT_CONFIG };
    }

    const fileExists = await exists(configPath);
    if (!fileExists) {
      console.log('⚙️  Config file does not exist, using default config');
      return { ...DEFAULT_CONFIG };
    }

    const contents = await readTextFile(configPath);
    const config = JSON.parse(contents) as LoomConfig;
    console.log('⚙️  Loaded config:', config);
    return config;
  } catch (error) {
    console.error('⚙️  Failed to load config:', error);
    return { ...DEFAULT_CONFIG };
  }
}

/**
 * Save config to .loom/config.json
 */
export async function saveConfig(config: LoomConfig): Promise<void> {
  try {
    const configPath = await getConfigPath();
    if (!configPath) {
      console.log('⚙️  No workspace set, skipping config save');
      return;
    }

    // Ensure .loom directory exists
    const loomDir = await join(cachedWorkspacePath!, '.loom');
    const loomDirExists = await exists(loomDir);
    if (!loomDirExists) {
      console.log('⚙️  Creating .loom directory');
      await createDir(loomDir, { recursive: true });
    }

    const contents = JSON.stringify(config, null, 2);
    await writeTextFile(configPath, contents);
    console.log('⚙️  Saved config:', config);
  } catch (error) {
    console.error('⚙️  Failed to save config:', error);
  }
}
