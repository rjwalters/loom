/**
 * Shared configuration utilities for Loom MCP server
 *
 * Handles workspace path resolution, state file reading/writing,
 * and config file operations.
 */

import { existsSync, readFileSync, statSync } from "node:fs";
import { access, readFile, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
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
 * Resolve the Loom repo root that owns a linked git worktree.
 *
 * A linked worktree's `.git` is a FILE containing `gitdir: <path>` that points
 * into the main checkout's `.git/worktrees/<name>` directory. The main checkout
 * root is therefore the grandparent of that gitdir (`…/.git/worktrees/<name>` →
 * `…/.git` → `…`). This mirrors `resolve_mcp_workspace()` /
 * `find_repo_root()` in the shell scripts (`defaults/scripts/claude-wrapper.sh`,
 * `defaults/scripts/cli/loom-daemon-update.sh`) so the TS and Bash resolvers
 * agree on which repo a worktree CWD maps to. Returns `null` when the `.git`
 * file is unparseable.
 */
function resolveWorktreeRoot(worktreeDir: string, gitFilePath: string): string | null {
  let content: string;
  try {
    content = readFileSync(gitFilePath, "utf-8");
  } catch {
    return null;
  }
  const match = content.match(/^gitdir:\s*(.+)$/m);
  if (!match) {
    return null;
  }
  let gitdir = match[1].trim();
  if (!isAbsolute(gitdir)) {
    // `gitdir:` may be recorded relative to the worktree directory.
    gitdir = resolve(worktreeDir, gitdir);
  }
  // gitdir = <main>/.git/worktrees/<name>  →  <main>/.git  →  <main>
  const commonGitDir = dirname(dirname(gitdir));
  const root = dirname(commonGitDir);
  return root || null;
}

/**
 * Discover the Loom repo root by walking up from a starting directory.
 *
 * Precedence within each directory mirrors the shell `loom_detect_context`
 * ordering (`scripts/loom`): a linked-worktree `.git` FILE is checked BEFORE the
 * `.loom/` marker, because a repo that commits `.loom/` (the norm) carries a
 * real `.loom/` directory inside every worktree checkout too — testing `.loom/`
 * first would misclassify every worktree as its own root instead of resolving to
 * the main checkout where `.loom/config.json` actually lives.
 *
 * Returns the resolved repo root, or `null` when no `.loom/`/`.git` marker is
 * found before reaching the filesystem root. Callers translate `null` into a
 * loud failure — there is deliberately NO silent `~/GitHub/loom` fallback (under
 * user-scope MCP registration a single server instance serves every repo, so a
 * hardcoded fallback would silently operate on the wrong repo).
 */
export function discoverWorkspaceRoot(startDir: string): string | null {
  let dir = startDir;
  // Bound the walk defensively; a normal path has far fewer components.
  for (let i = 0; i < 256; i++) {
    const gitPath = join(dir, ".git");
    // Linked worktree: `.git` is a FILE. Resolve to the main checkout FIRST.
    if (existsSync(gitPath) && statSync(gitPath).isFile()) {
      const root = resolveWorktreeRoot(dir, gitPath);
      if (root) {
        return root;
      }
      // Unparseable `.git` file — treat this dir as the root rather than
      // silently walking past a real repo boundary.
      return dir;
    }
    // Consumer repo / source checkout: `.loom/` marks the root.
    if (existsSync(join(dir, ".loom")) && statSync(join(dir, ".loom")).isDirectory()) {
      return dir;
    }
    // Plain git repo with no committed `.loom/`: `.git` directory marks the root.
    if (existsSync(gitPath) && statSync(gitPath).isDirectory()) {
      return dir;
    }
    const parent = dirname(dir);
    if (parent === dir) {
      break; // reached the filesystem root
    }
    dir = parent;
  }
  return null;
}

/**
 * Get the workspace path (Loom repo root the server operates on).
 *
 * Resolution order:
 *   1. `LOOM_WORKSPACE` env override (highest precedence — explicit, keep).
 *   2. CWD-based discovery: walk up from `process.cwd()` to a repo root
 *      (`.loom/` or `.git` marker; worktree CWDs resolve to the main checkout).
 *   3. Loud failure — throws. There is NO silent `~/GitHub/loom` fallback: under
 *      user-scope MCP registration a single server serves every repo (issue
 *      #4230, epic #3835 Phase 3c), so falling back to a hardcoded path would
 *      silently operate on the WRONG repo — strictly worse than an error.
 */
export function getWorkspacePath(): string {
  const explicit = process.env.LOOM_WORKSPACE;
  if (explicit && explicit.trim() !== "") {
    return explicit;
  }
  const cwd = process.cwd();
  const discovered = discoverWorkspaceRoot(cwd);
  if (discovered) {
    return discovered;
  }
  const message =
    `[mcp-loom] Could not resolve a Loom workspace: LOOM_WORKSPACE is unset and no ` +
    `.loom/ or .git repo root was found by walking up from ${cwd}. ` +
    `Run the server from inside a Loom repo, or set LOOM_WORKSPACE explicitly. ` +
    `(No silent ~/GitHub/loom fallback — see issue #4230.)`;
  process.stderr.write(`${message}\n`);
  throw new Error(message);
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
