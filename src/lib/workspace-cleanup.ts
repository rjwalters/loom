import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import type { AppState } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { cleanupAllTerminalWorktrees } from "./terminal-worktree-manager";

/**
 * Options for workspace cleanup operations
 */
export interface WorkspaceCleanupOptions {
  /**
   * Component name for structured logging (e.g., "workspace-start", "workspace-reset")
   */
  component: string;

  /**
   * Application state instance
   */
  state: AppState;

  /**
   * Output poller instance for stopping polling
   */
  outputPoller: OutputPoller;

  /**
   * Terminal manager instance for destroying xterm instances
   */
  terminalManager: TerminalManager;

  /**
   * Callback to clear the currently attached terminal ID
   */
  setCurrentAttachedTerminalId: (id: string | null) => void;

  /**
   * Workspace path (optional) - if provided, will clean up terminal worktrees
   */
  workspacePath?: string;
}

/**
 * Performs complete workspace cleanup before starting or resetting terminals.
 *
 * This function executes the following cleanup steps in order:
 * 1. Stops output polling for all terminals
 * 2. Destroys all xterm instances
 * 3. Destroys all terminal sessions in the daemon
 * 4. Kills all loom tmux sessions to ensure clean slate
 * 5. Cleans up terminal worktrees (if workspacePath provided)
 * 6. Clears terminal state
 *
 * This cleanup sequence is used by both workspace-start and workspace-reset
 * to ensure a clean state before creating new terminals.
 *
 * @param options - Cleanup options including component name and dependencies
 *
 * @example
 * ```typescript
 * await cleanupWorkspace({
 *   component: "workspace-start",
 *   state,
 *   outputPoller,
 *   terminalManager,
 *   setCurrentAttachedTerminalId,
 * });
 * ```
 */
export async function cleanupWorkspace(options: WorkspaceCleanupOptions): Promise<void> {
  const {
    component,
    state,
    outputPoller,
    terminalManager,
    setCurrentAttachedTerminalId,
    workspacePath,
  } = options;

  const logger = Logger.forComponent(component);

  // Get terminals before cleanup
  const terminals = state.getTerminals();

  // Stop all polling
  logger.info("Stopping output polling for all terminals", {
    terminalCount: terminals.length,
  });
  terminals.forEach((t) => outputPoller.stopPolling(t.id));

  // Destroy all xterm instances
  logger.info("Destroying xterm instances");
  terminalManager.destroyAll();

  // Destroy all terminal sessions in daemon (clean up old tmux sessions)
  logger.info("Destroying terminal sessions", {
    terminalCount: terminals.length,
  });
  for (const terminal of terminals) {
    try {
      await invoke("destroy_terminal", { id: terminal.id });
      logger.info("Destroyed terminal session", {
        terminalId: terminal.id,
        terminalName: terminal.name,
      });
    } catch (error) {
      logger.error("Failed to destroy terminal", error, {
        terminalId: terminal.id,
      });
      // Continue anyway - we'll create fresh terminals
    }
  }

  // Kill ALL loom tmux sessions to ensure clean slate
  logger.info("Killing all loom tmux sessions");
  try {
    await invoke("kill_all_loom_sessions");
    logger.info("All loom sessions killed");
  } catch (error) {
    logger.error("Failed to kill loom sessions", error);
    // Continue anyway - we'll try to create fresh terminals
  }

  // Clean up terminal worktrees if workspace path provided
  if (workspacePath) {
    logger.info("Cleaning up terminal worktrees", {
      workspacePath,
    });
    try {
      await cleanupAllTerminalWorktrees(workspacePath);
      logger.info("Terminal worktrees cleaned up");
    } catch (error) {
      logger.error("Failed to clean up terminal worktrees", error, {
        workspacePath,
      });
      // Continue anyway - cleanup failures are non-critical
    }
  }

  // Clear state (but don't clear config files)
  logger.info("Clearing terminals from state");
  state.clearAll();
  setCurrentAttachedTerminalId(null);

  logger.info("Workspace cleanup complete");
}
