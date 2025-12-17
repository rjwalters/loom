/**
 * Terminal Action Handlers
 *
 * Functions for handling terminal-specific UI actions like running interval prompts,
 * restarting terminals, and inline renaming. These are called from event handlers in main.ts.
 */

import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { type AppLevelState, appLevelState } from "./app-state";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import { announceTerminalCreated, announceTerminalRemoved } from "./screen-reader-announcer";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { showToast } from "./toast";

const logger = Logger.forComponent("terminal-actions");

/**
 * Dependencies required by terminal action handlers
 */
export interface TerminalActionDependencies {
  state: AppState;
  saveCurrentConfig: () => Promise<void>;
  render: () => void;
}

/**
 * Dependencies required for closing terminals
 */
export interface CloseTerminalDependencies {
  state: AppState;
  outputPoller: OutputPoller;
  terminalManager: TerminalManager;
  appLevelState: AppLevelState;
  saveCurrentConfig: () => Promise<void>;
}

/**
 * Handle Run Now button click for interval mode terminals
 *
 * Executes the configured interval prompt immediately and resets the timer.
 * This allows users to manually trigger autonomous terminal actions on-demand.
 *
 * @param terminalId - The ID of the terminal to run
 * @param deps - Dependencies (state)
 */
export async function handleRunNowClick(
  terminalId: string,
  deps: Pick<TerminalActionDependencies, "state">
): Promise<void> {
  const { state } = deps;
  logger.info("Running interval prompt manually", { terminalId });

  try {
    const terminal = state.getTerminal(terminalId);
    if (!terminal) {
      logger.error("Terminal not found", new Error("Terminal not found"), {
        terminalId,
      });
      return;
    }

    // Import interval prompt manager
    const { getIntervalPromptManager } = await import("./interval-prompt-manager");
    const intervalManager = getIntervalPromptManager();

    // Execute the interval prompt
    await intervalManager.runNow(terminal);
    logger.info("Successfully executed interval prompt", { terminalId });
  } catch (error) {
    logger.error("Failed to execute interval prompt", error as Error, {
      terminalId,
    });
    showToast(`Failed to run interval prompt: ${error}`, "error");
  }
}

/**
 * Handle Restart Terminal button click
 *
 * Destroys the current terminal and creates a new one with the same configuration.
 * If the terminal had an agent running, it will be relaunched.
 *
 * @param terminalId - The ID of the terminal to restart
 * @param deps - Dependencies (state, saveCurrentConfig)
 */
export async function handleRestartTerminal(
  terminalId: string,
  deps: Pick<TerminalActionDependencies, "state" | "saveCurrentConfig">
): Promise<void> {
  const { state, saveCurrentConfig } = deps;
  logger.info("Restarting terminal", { terminalId });

  try {
    const terminal = state.getTerminal(terminalId);
    if (!terminal) {
      logger.error("Terminal not found", new Error("Terminal not found"), {
        terminalId,
      });
      return;
    }

    // Store terminal configuration before destroying
    const config = {
      name: terminal.name,
      role: terminal.role,
      roleConfig: terminal.roleConfig,
      worktreePath: terminal.worktreePath,
    };

    logger.info("Stored terminal configuration for restart", {
      terminalId,
      config,
    });

    // Set terminal to busy status during restart
    state.updateTerminal(terminalId, { status: TerminalStatus.Busy });

    // Destroy the terminal via Tauri IPC
    await invoke("destroy_terminal", { id: terminalId });

    logger.info("Destroyed terminal", { terminalId });

    // Small delay to ensure tmux session is fully cleaned up
    await new Promise((resolve) => setTimeout(resolve, 500));

    // Create a new terminal with the same configuration
    const workspacePath = state.getWorkspace();
    if (!workspacePath) {
      throw new Error("No workspace path available");
    }

    // Determine working directory - use worktree path if available, otherwise workspace
    const workingDir = config.worktreePath || workspacePath;

    await invoke("create_terminal", {
      configId: terminalId,
      name: config.name,
      workingDir,
      role: config.role || "",
      instanceNumber: 0,
    });

    logger.info("Created new terminal", { terminalId });

    // If terminal had a worktree path, set it again
    if (config.worktreePath) {
      await invoke("set_worktree_path", {
        id: terminalId,
        worktreePath: config.worktreePath,
      });
    }

    // Update terminal status to idle
    state.updateTerminal(terminalId, { status: TerminalStatus.Idle });

    // Save the configuration
    await saveCurrentConfig();

    // If terminal had a role config with an agent, relaunch it
    if (config.role === "claude-code-worker" && config.roleConfig?.roleFile) {
      logger.info("Relaunching agent for restarted terminal", { terminalId });

      const { launchAgentsForTerminals } = await import("./terminal-lifecycle");
      await launchAgentsForTerminals(workspacePath, [terminal], {
        state,
        saveCurrentConfig,
      });
    }

    logger.info("Successfully restarted terminal", { terminalId });
  } catch (error) {
    logger.error("Failed to restart terminal", error as Error, {
      terminalId,
    });
    // Reset status to idle on error
    state.updateTerminal(terminalId, { status: TerminalStatus.Idle });
    showToast(`Failed to restart terminal: ${error}`, "error");
  }
}

/**
 * Start inline renaming for a terminal
 *
 * Replaces the terminal name element with an inline text input for editing.
 * Handles commit (Enter key or blur) and cancel (Escape key) actions.
 *
 * @param terminalId - The ID of the terminal to rename
 * @param nameElement - The DOM element containing the terminal name
 * @param deps - Dependencies (state, saveCurrentConfig)
 */
export function startRename(
  terminalId: string,
  nameElement: HTMLElement,
  deps: TerminalActionDependencies
): void {
  const { state, saveCurrentConfig, render } = deps;
  const terminal = state.getTerminals().find((t) => t.id === terminalId);
  if (!terminal) return;

  const currentName = terminal.name;
  const input = document.createElement("input");
  input.type = "text";
  input.value = currentName;

  // Match the font size of the original element
  const fontSize = nameElement.classList.contains("text-sm") ? "text-sm" : "text-xs";
  input.className = `px-1 bg-white dark:bg-gray-900 border border-blue-500 rounded ${fontSize} font-medium w-full`;

  // Replace the name element with input
  const parent = nameElement.parentElement;
  if (!parent) return;

  parent.replaceChild(input, nameElement);

  // Defer focus to the next tick to prevent the double-click event from interfering
  setTimeout(() => {
    input.focus();
    input.select();
    // Set editing flag to prevent timer-based re-renders from discarding edits
    appLevelState.isUserEditing = true;
  }, 0);

  const commit = () => {
    // Clear editing flag before committing
    appLevelState.isUserEditing = false;

    const newName = input.value.trim();
    if (newName && newName !== currentName) {
      state.renameTerminal(terminalId, newName);
      saveCurrentConfig();
    } else {
      // Just re-render to restore the original name element
      render();
    }
  };

  const cancel = () => {
    // Clear editing flag before cancelling
    appLevelState.isUserEditing = false;
    render();
  };

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    }
  });

  input.addEventListener("blur", () => {
    commit();
  });
}

/**
 * Close a terminal with confirmation dialog
 *
 * Shows a confirmation dialog before closing the terminal. If confirmed:
 * - Stops autonomous mode if running
 * - Stops output polling
 * - Destroys the terminal session
 * - Clears attached terminal ID if needed
 * - Removes terminal from state
 * - Saves configuration
 *
 * @param terminalId - The ID of the terminal to close
 * @param deps - Dependencies (state, outputPoller, terminalManager, appLevelState, saveCurrentConfig)
 * @returns Promise that resolves when the terminal is closed or user cancels
 */
export async function closeTerminalWithConfirmation(
  terminalId: string,
  deps: CloseTerminalDependencies
): Promise<void> {
  const { state, outputPoller, terminalManager, appLevelState, saveCurrentConfig } = deps;

  const terminal = state.getTerminal(terminalId);
  if (!terminal) {
    logger.error("Terminal not found", new Error("Terminal not found"), {
      terminalId,
    });
    return;
  }

  // Ask for confirmation
  const confirmed = await ask(`Are you sure you want to close "${terminal.name}"?`, {
    title: "Close Terminal",
    kind: "warning",
  });

  if (!confirmed) {
    return;
  }

  logger.info("Closing terminal", { terminalId });

  // Stop interval prompts if running
  const { getIntervalPromptManager } = await import("./interval-prompt-manager");
  const intervalManager = getIntervalPromptManager();
  intervalManager.stop(terminalId);

  // Stop polling and destroy terminal
  outputPoller.stopPolling(terminalId);
  terminalManager.destroyTerminal(terminalId);

  // Get terminal name before removing
  const terminalToRemove = state.getTerminals().find((t) => t.id === terminalId);
  const terminalName = terminalToRemove?.name || "Unknown";

  // Clear attached terminal ID if it matches
  if (appLevelState.currentAttachedTerminalId === terminalId) {
    appLevelState.currentAttachedTerminalId = null;
  }

  // Remove from state
  state.removeTerminal(terminalId);

  // Announce terminal removal to screen readers
  announceTerminalRemoved(terminalName);

  // Save configuration
  await saveCurrentConfig();

  logger.info("Terminal closed successfully", { terminalId });
}

/**
 * Create a plain shell terminal
 *
 * Creates a new terminal in the workspace directory with the default role.
 * Automatically generates a worktree for the terminal and switches to it.
 *
 * @param deps - Dependencies for terminal creation
 */
export async function createPlainTerminal(deps: {
  state: AppState;
  workspacePath: string;
  generateNextConfigId: (terminals: Terminal[]) => string;
  saveCurrentConfig: () => Promise<void>;
}): Promise<Terminal | undefined> {
  const { state, workspacePath, generateNextConfigId, saveCurrentConfig } = deps;

  // Generate terminal name
  const terminalCount = state.getTerminals().length + 1;
  const name = `Terminal ${terminalCount}`;

  try {
    // Generate stable ID first
    const id = generateNextConfigId(state.getTerminals());

    // Get instance number for this terminal
    const instanceNumber = state.getNextTerminalNumber();

    // Create worktree for this terminal FIRST (before creating terminal)
    logger.info("Creating worktree for terminal", { name, id });
    const { setupWorktreeForAgent } = await import("./worktree-manager");
    const worktreePath = await setupWorktreeForAgent(id, workspacePath);
    logger.info("Created worktree", { name, id, worktreePath });

    // Create terminal in worktree directory (not workspace)
    const terminalId = await invoke<string>("create_terminal", {
      configId: id,
      name,
      workingDir: worktreePath,
      role: "default",
      instanceNumber,
    });

    logger.info("Created terminal", { name, id, tmuxId: terminalId, workingDir: worktreePath });

    // Add to state with default role (plain shell / driver)
    const newTerminal: Terminal = {
      id,
      name,
      worktreePath,
      status: TerminalStatus.Idle,
      isPrimary: false,
      role: "default",
      theme: "default",
    };
    state.addTerminal(newTerminal);

    // Announce terminal creation to screen readers
    announceTerminalCreated(name);

    // Save updated state to config
    await saveCurrentConfig();

    // Switch to new terminal
    state.setPrimary(id);

    // Return the created terminal
    return newTerminal;
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    logger.error("Failed to create terminal", error, { workspacePath, errorMessage });
    showToast(`Failed to create terminal: ${errorMessage}`, "error");
    return undefined;
  }
}
