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
import { resolveEffectiveConfig } from "./config-resolver.js";

/** Global Loom directory in user's home */
export const LOOM_DIR = join(homedir(), ".loom");

/** Daemon log file path */
export const DAEMON_LOG = join(LOOM_DIR, "daemon.log");

/** Browser console log file path */
export const CONSOLE_LOG_PATH = join(LOOM_DIR, "console.log");

/** Global state file path */
export const STATE_FILE = join(LOOM_DIR, "state.json");

/** MCP command file for file-based IPC */
export const MCP_COMMAND_FILE = join(LOOM_DIR, "mcp-command.json");

/** MCP acknowledgment file for file-based IPC */
export const MCP_ACK_FILE = join(LOOM_DIR, "mcp-ack.json");

/** Daemon socket path (can be overridden via LOOM_SOCKET_PATH env var) */
export const SOCKET_PATH = process.env.LOOM_SOCKET_PATH || join(LOOM_DIR, "loom-daemon.sock");

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
 * Read the effective workspace config.
 *
 * Resolves the full tier chain (private defaults → `.loom/config.json` →
 * `.loom-project/project.json` → `.loom-local/local.json`) via
 * {@link resolveEffectiveConfig}, so every read surface agrees with the
 * daemon / Python / Bash resolvers about the effective config (issue #4064).
 *
 * Returns `null` when no tier contributes any content (an uninitialized
 * workspace), preserving the previous "config file not found" contract. When
 * only the legacy `.loom/config.json` is present — the state of every repo
 * today — the merged result is byte-for-byte that file's parsed content.
 *
 * WRITE HAZARD: the result may be a merge across tiers. Callers must NOT write
 * it back to `.loom/config.json` when a non-legacy tier is present, or they
 * flatten higher-tier overrides into the legacy file. `configure_terminal`
 * guards against this with `hasNonLegacyTier` (see config-resolver.ts).
 */
export async function readConfigFile(): Promise<ConfigFile | null> {
  const effective = await resolveEffectiveConfig(getWorkspacePath());
  if (Object.keys(effective).length === 0) {
    return null;
  }
  return effective as unknown as ConfigFile;
}

/**
 * Write the workspace config file.
 *
 * Always targets the legacy `.loom/config.json` and only that file — this
 * resolver family is read-only across all four languages, so there is no
 * tier-aware write path. Do not call this with a merged (multi-tier) config
 * when a non-legacy tier is present; see `readConfigFile`'s WRITE HAZARD note
 * and the `hasNonLegacyTier` guard in `configure_terminal`.
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
