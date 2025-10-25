import { invoke } from "@tauri-apps/api/core";
import { Command } from "@tauri-apps/plugin-shell";
import { Logger } from "./logger";

const logger = Logger.forComponent("worktree-manager");

/**
 * Generate a short hash from workspace path for /tmp directory isolation
 * Uses simple string hash to create 8-character identifier
 */
function hashWorkspacePath(path: string): string {
  let hash = 0;
  for (let i = 0; i < path.length; i++) {
    const char = path.charCodeAt(i);
    hash = (hash << 5) - hash + char;
    hash = hash & hash; // Convert to 32-bit integer
  }
  return Math.abs(hash).toString(36).substring(0, 8);
}

export interface GitIdentity {
  name: string;
  email: string;
}

/**
 * worktree-manager.ts - Functions for setting up git worktrees for agent isolation
 *
 * IMPORTANT: All functions in this module operate on sessionIds (ephemeral tmux session IDs).
 * - terminalId parameters are sessionIds used for terminal IPC operations (send_input)
 * - Returns worktreePath which caller stores in state using configId
 */

/**
 * Set up a terminal worktree with role-specific CLAUDE.md BEFORE terminal creation
 *
 * This creates a git worktree for a terminal with the role configuration copied
 * to CLAUDE.md. This must be called BEFORE the terminal is created, since the
 * terminal needs to start in this worktree directory.
 *
 * Unlike setupWorktreeForAgent (which sends commands to an existing terminal),
 * this function creates the worktree directly using shell commands.
 *
 * @param terminalId - The terminal ID (e.g., "terminal-1")
 * @param workspacePath - The main workspace path (git repository root)
 * @param roleFile - The role file name (e.g., "curator.md")
 * @returns Promise that resolves with the worktree path
 */
export async function setupTerminalWorktree(
  terminalId: string,
  workspacePath: string,
  roleFile: string
): Promise<string> {
  logger.info("Creating terminal worktree", { terminalId, workspacePath, roleFile });

  // Use /tmp for terminal worktrees to avoid Tauri filesystem scope restrictions
  // Hash workspace path to avoid conflicts between different repos
  const workspaceHash = hashWorkspacePath(workspacePath);
  const worktreePath = `/tmp/loom-worktrees/${workspaceHash}/${terminalId}`;
  const branchName = `worktree/${terminalId}`;

  logger.info("Terminal worktree path", { worktreePath, workspaceHash });

  try {
    // Create /tmp/loom-worktrees/{hash} directory if it doesn't exist
    const worktreesBaseDir = `/tmp/loom-worktrees/${workspaceHash}`;
    const mkdirCmd = Command.create("mkdir", ["-p", worktreesBaseDir], { cwd: workspacePath });
    const mkdirResult = await mkdirCmd.execute();
    if (mkdirResult.code !== 0) {
      logger.warn("mkdir failed but continuing", { stderr: mkdirResult.stderr });
    } else {
      logger.info("Created worktrees base directory", { worktreesBaseDir });
    }

    // Check if worktree already exists - if so, remove it first
    const checkCmd = Command.create("test", ["-d", worktreePath], { cwd: workspacePath });
    const checkResult = await checkCmd.execute();

    if (checkResult.code === 0) {
      logger.warn("Terminal worktree already exists, removing it", { worktreePath });
      const removeCmd = Command.create("git", ["worktree", "remove", worktreePath, "--force"], {
        cwd: workspacePath,
      });
      await removeCmd.execute();
    }

    // Create git worktree with a unique branch
    logger.info("Creating git worktree", { worktreePath, branchName });
    const addCmd = Command.create(
      "git",
      ["worktree", "add", "-b", branchName, worktreePath, "HEAD"],
      { cwd: workspacePath }
    );
    const result = await addCmd.execute();

    if (result.code !== 0) {
      throw new Error(`Failed to create worktree: ${result.stderr}`);
    }

    // Copy role file to CLAUDE.md using cp command to avoid FS API restrictions
    const roleFilePath = `${workspacePath}/.loom/roles/${roleFile}`;
    const claudeMdPath = `${worktreePath}/CLAUDE.md`;
    logger.info("Copying role file to CLAUDE.md", { roleFilePath, claudeMdPath });

    const cpCmd = Command.create("cp", [roleFilePath, claudeMdPath], { cwd: workspacePath });
    const cpResult = await cpCmd.execute();

    if (cpResult.code !== 0) {
      logger.error("Failed to copy role file", null, {
        roleFilePath,
        claudeMdPath,
        stderr: cpResult.stderr,
      });
      throw new Error(`Failed to copy role file: ${cpResult.stderr}`);
    }

    logger.info("Terminal worktree created successfully", {
      terminalId,
      worktreePath,
      roleFile,
    });

    return worktreePath;
  } catch (error) {
    logger.error("Failed to create terminal worktree", error, {
      terminalId,
      workspacePath,
      roleFile,
      worktreePath,
    });
    throw error;
  }
}

/**
 * Set up a git worktree for an agent terminal
 *
 * This creates an isolated git worktree for the agent to work in, preventing
 * conflicts between multiple agents working on different features.
 *
 * The worktree is created by sending git commands directly to the terminal,
 * making the process visible to the user.
 *
 * @param terminalId - The terminal ID to set up the worktree in
 * @param workspacePath - The main workspace path (git repository root)
 * @param gitIdentity - Optional git identity to configure in the worktree
 * @returns Promise that resolves with the worktree path
 */
export async function setupWorktreeForAgent(
  terminalId: string,
  workspacePath: string,
  gitIdentity?: GitIdentity
): Promise<string> {
  logger.info("Starting worktree setup", { terminalId, workspacePath });

  // Worktree path: .loom/worktrees/{terminalId}
  const worktreePath = `${workspacePath}/.loom/worktrees/${terminalId}`;

  try {
    // Create worktrees directory if it doesn't exist
    await sendCommand(terminalId, `mkdir -p "${worktreePath}"`);

    // Create git worktree with a unique branch name
    // This ensures proper isolation - git prevents checking out a branch
    // that's already checked out in another worktree (including main repo)
    const branchName = `worktree/${terminalId}`;
    await sendCommand(terminalId, `git worktree add -b "${branchName}" "${worktreePath}" HEAD`);

    // Change to worktree directory
    await sendCommand(terminalId, `cd "${worktreePath}"`);

    // Configure git identity if provided
    if (gitIdentity) {
      logger.info("Configuring git identity", {
        terminalId,
        gitName: gitIdentity.name,
        gitEmail: gitIdentity.email,
      });
      await sendCommand(terminalId, `git config user.name "${gitIdentity.name}"`);
      await sendCommand(terminalId, `git config user.email "${gitIdentity.email}"`);
      await sendCommand(
        terminalId,
        `echo "✓ Git identity configured: ${gitIdentity.name} <${gitIdentity.email}>"`
      );
    }

    // Log success message
    await sendCommand(terminalId, `echo "✓ Worktree ready at ${worktreePath}"`);

    // Notify daemon about worktree path for reference counting
    try {
      await invoke("set_worktree_path", {
        id: terminalId,
        worktreePath,
      });
      logger.info("Notified daemon about worktree", { terminalId, worktreePath });
    } catch (error) {
      logger.error("Failed to notify daemon about worktree path", error, {
        terminalId,
        worktreePath,
      });
      // Non-fatal - continue even if notification fails
    }

    logger.info("Worktree setup complete", { terminalId, worktreePath });
    return worktreePath;
  } catch (error) {
    logger.error("Failed to setup worktree", error, {
      terminalId,
      workspacePath,
      worktreePath,
    });
    throw error;
  }
}

/**
 * Clean up all terminal worktrees in /tmp/loom-worktrees/{hash}/terminal-*
 *
 * This function removes all terminal worktrees (not issue worktrees).
 * It's called during factory reset to ensure a clean slate.
 *
 * @param workspacePath - The main workspace path (git repository root)
 */
export async function cleanupTerminalWorktrees(workspacePath: string): Promise<void> {
  logger.info("Cleaning up terminal worktrees", { workspacePath });

  const workspaceHash = hashWorkspacePath(workspacePath);
  const tmpWorktreesDir = `/tmp/loom-worktrees/${workspaceHash}`;

  logger.info("Terminal worktrees cleanup location", { tmpWorktreesDir });

  try {
    // List all git worktrees
    const listCmd = Command.create("git", ["worktree", "list", "--porcelain"], {
      cwd: workspacePath,
    });
    const result = await listCmd.execute();

    if (result.code !== 0) {
      logger.error("Failed to list worktrees", new Error(result.stderr), { workspacePath });
      return;
    }

    // Parse worktree list to find terminal worktrees in /tmp
    const lines = result.stdout.split("\n");
    const worktreePaths: string[] = [];

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      if (line.startsWith("worktree ")) {
        const path = line.substring("worktree ".length);
        // Only remove terminal worktrees from /tmp, not issue worktrees
        if (path.includes(`/tmp/loom-worktrees/${workspaceHash}/terminal-`)) {
          worktreePaths.push(path);
        }
      }
    }

    // Remove each terminal worktree
    for (const path of worktreePaths) {
      try {
        logger.info("Removing terminal worktree", { path });

        // Extract terminal ID from path
        const match = path.match(/terminal-\d+/);
        const terminalId = match ? match[0] : null;

        // Remove worktree
        const removeCmd = Command.create("git", ["worktree", "remove", path, "--force"], {
          cwd: workspacePath,
        });
        await removeCmd.execute();

        // Remove associated branch if we found the terminal ID
        if (terminalId) {
          const branchName = `worktree/${terminalId}`;
          logger.info("Removing worktree branch", { branchName });

          const branchCmd = Command.create("git", ["branch", "-D", branchName], {
            cwd: workspacePath,
          });
          await branchCmd.execute();
        }

        logger.info("Terminal worktree removed", { path, terminalId: terminalId || undefined });
      } catch (error) {
        logger.error("Failed to remove terminal worktree", error, { path });
        // Continue with other worktrees
      }
    }

    logger.info("Terminal worktree cleanup complete", {
      workspacePath,
      removedCount: worktreePaths.length,
    });
  } catch (error) {
    logger.error("Failed to cleanup terminal worktrees", error, { workspacePath });
  }
}

/**
 * Note: Worktree cleanup is handled by the Rust daemon when terminals are destroyed.
 * The daemon checks if the working_dir contains ".loom/worktrees" and automatically
 * removes the worktree via `git worktree remove` and fallback `rm -rf`.
 *
 * IMPORTANT: The daemon must also clean up the worktree branch created for isolation.
 * When destroying a terminal, the daemon should run:
 *   1. `git worktree remove ${worktreePath} --force`
 *   2. Extract terminalId from worktreePath (e.g., ".loom/worktrees/terminal-1" → "terminal-1")
 *   3. `git branch -D worktree/${terminalId}`
 *
 * This ensures both the worktree directory AND the associated branch are removed,
 * preventing branch accumulation over time.
 *
 * See loom-daemon/src/terminal.rs:destroy_terminal for implementation.
 */

/**
 * Send a command to a terminal and wait for it to execute
 *
 * @param terminalId - The terminal ID to send the command to
 * @param command - The command to execute
 */
async function sendCommand(terminalId: string, command: string): Promise<void> {
  // Send command
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Delay to allow command to fully execute before sending next command
  // This prevents command concatenation in the terminal
  await new Promise((resolve) => setTimeout(resolve, 300));
}
