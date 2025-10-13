import { invoke } from "@tauri-apps/api/tauri";

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
 * @returns Promise that resolves with the worktree path
 */
export async function setupWorktreeForAgent(
  terminalId: string,
  workspacePath: string
): Promise<string> {
  // Worktree path: .loom/worktrees/{terminalId}
  const worktreePath = `${workspacePath}/.loom/worktrees/${terminalId}`;

  // Create worktrees directory if it doesn't exist
  await sendCommand(terminalId, `mkdir -p "${worktreePath}"`);

  // Create git worktree (visible to user in terminal)
  // Using HEAD to branch from current commit
  await sendCommand(terminalId, `git worktree add "${worktreePath}" HEAD`);

  // Change to worktree directory
  await sendCommand(terminalId, `cd "${worktreePath}"`);

  // Log success message
  await sendCommand(terminalId, `echo "✓ Worktree ready at ${worktreePath}"`);

  return worktreePath;
}

/**
 * Note: Worktree cleanup is handled by the Rust daemon when terminals are destroyed.
 * The daemon checks if the working_dir contains ".loom/worktrees" and automatically
 * removes the worktree via `git worktree remove` and fallback `rm -rf`.
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

  // Small delay to allow command to start executing
  // (user will see output in terminal)
  await new Promise((resolve) => setTimeout(resolve, 100));
}
