/**
 * Shared configuration utilities for Loom MCP server
 *
 * Handles workspace path resolution, state file reading/writing,
 * and config file operations.
 */

import { access, readFile, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ConfigFile, StateFile } from "../types.js";

/** Global Loom directory in user's home */
export const LOOM_DIR = join(homedir(), ".loom");

/** Daemon log file path */
export const DAEMON_LOG = join(LOOM_DIR, "daemon.log");

/** Tauri application log file path */
export const TAURI_LOG = join(LOOM_DIR, "tauri.log");

/** Browser console log file path */
export const CONSOLE_LOG_PATH = join(LOOM_DIR, "console.log");

/** Global state file path */
export const STATE_FILE = join(LOOM_DIR, "state.json");

/** MCP command file for file-based IPC */
export const MCP_COMMAND_FILE = join(LOOM_DIR, "mcp-command.json");

/** MCP acknowledgment file for file-based IPC */
export const MCP_ACK_FILE = join(LOOM_DIR, "mcp-ack.json");

/** Daemon socket path (can be overridden via LOOM_SOCKET_PATH env var) */
export const SOCKET_PATH =
  process.env.LOOM_SOCKET_PATH || join(LOOM_DIR, "loom-daemon.sock");

/**
 * Get the workspace path from environment or default
 */
export function getWorkspacePath(): string {
  return process.env.LOOM_WORKSPACE || join(homedir(), "GitHub", "loom");
}

/**
 * Read a file and check if it exists
 */
export async function readFileIfExists(filePath: string): Promise<string | null> {
  try {
    await access(filePath);
    return await readFile(filePath, "utf-8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * Read the global state file
 */
export async function readStateFile(): Promise<StateFile | null> {
  try {
    const fileStats = await stat(STATE_FILE);
    if (!fileStats.isFile()) {
      return null;
    }

    const content = await readFile(STATE_FILE, "utf-8");
    return JSON.parse(content) as StateFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * Write the global state file
 */
export async function writeStateFile(state: StateFile): Promise<void> {
  state.lastUpdated = new Date().toISOString();
  await writeFile(STATE_FILE, JSON.stringify(state, null, 2), "utf-8");
}

/**
 * Read the workspace config file
 */
export async function readConfigFile(): Promise<ConfigFile | null> {
  try {
    const workspacePath = getWorkspacePath();
    const configPath = join(workspacePath, ".loom", "config.json");

    const fileStats = await stat(configPath);
    if (!fileStats.isFile()) {
      return null;
    }

    const content = await readFile(configPath, "utf-8");
    return JSON.parse(content) as ConfigFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * Write the workspace config file
 */
export async function writeConfigFile(config: ConfigFile): Promise<void> {
  const workspacePath = getWorkspacePath();
  const configPath = join(workspacePath, ".loom", "config.json");
  await writeFile(configPath, JSON.stringify(config, null, 2), "utf-8");
}

/**
 * Read the workspace state file (returns as string for compatibility)
 */
export async function readWorkspaceStateFile(): Promise<string> {
  try {
    const workspacePath = getWorkspacePath();
    const statePath = join(workspacePath, ".loom", "state.json");

    await access(statePath);
    return await readFile(statePath, "utf-8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return "State file not found. Workspace may not be initialized.";
    }
    throw error;
  }
}

/**
 * Read the workspace config file (returns as string for compatibility)
 */
export async function readWorkspaceConfigFile(): Promise<string> {
  try {
    const workspacePath = getWorkspacePath();
    const configPath = join(workspacePath, ".loom", "config.json");

    await access(configPath);
    return await readFile(configPath, "utf-8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return "Config file not found. Workspace may not be initialized.";
    }
    throw error;
  }
}
