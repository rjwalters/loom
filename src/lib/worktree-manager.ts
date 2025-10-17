import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";

const logger = Logger.forComponent("worktree-manager");

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
