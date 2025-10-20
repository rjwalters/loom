/**
 * Terminal Session Recovery Handlers
 *
 * Functions for recovering terminals with missing tmux sessions.
 * These provide UI actions for creating new sessions or attaching to existing ones.
 */

import { ask } from "@tauri-apps/api/dialog";
import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";
import type { AppState } from "./state";
import { TerminalStatus } from "./state";

const logger = Logger.forComponent("recovery-handlers");

/**
 * Dependencies required by recovery handlers
 */
export interface RecoveryDependencies {
  state: AppState;
  generateNextConfigId: () => string;
  saveCurrentConfig: () => Promise<void>;
}

/**
 * Create a new session for a terminal with a missing session
 *
 * This recovery option creates a fresh tmux session and updates the terminal state.
 * The terminal gets a new config ID and is set as the primary terminal.
 *
 * @param terminalId - The ID of the terminal to recover
 * @param deps - Dependencies (state, generateNextConfigId, saveCurrentConfig)
 */
export async function handleRecoverNewSession(
  terminalId: string,
  deps: RecoveryDependencies
): Promise<void> {
  const { state, generateNextConfigId, saveCurrentConfig } = deps;
  logger.info("Creating new session for terminal", { terminalId });

  try {
    if (!state.hasWorkspace()) {
      alert("Cannot recover: no workspace selected");
      return;
    }

    const workspacePath = state.getWorkspaceOrThrow();
    const terminal = state.getTerminals().find((t) => t.id === terminalId);

    if (!terminal) {
      alert("Cannot recover: terminal not found");
      return;
    }

    // Get instance number
    const instanceNumber = state.getNextTerminalNumber();

    // Generate a new config ID for the recovered terminal
    const newConfigId = generateNextConfigId();

    // Create a new terminal in the daemon
    const newTerminalId = await invoke<string>("create_terminal", {
      configId: newConfigId,
      name: terminal.name,
      workingDir: workspacePath,
      role: terminal.role || "default",
      instanceNumber,
    });

    logger.info("Created new terminal", { oldTerminalId: terminalId, newTerminalId });

    // Update the terminal in state with the new ID
    state.removeTerminal(terminalId);
    state.addTerminal({
      ...terminal,
      id: newTerminalId,
      status: TerminalStatus.Idle,
      missingSession: undefined,
    });

    // Set as primary
    state.setPrimary(newTerminalId);

    // Save config
    await saveCurrentConfig();

    logger.info("Recovery complete", { terminalId: newTerminalId });
  } catch (error) {
    logger.error("Failed to recover terminal", error, { terminalId });
    alert(`Failed to create new session: ${error}`);
  }
}

/**
 * Show a list of available tmux sessions for attachment
 *
 * This recovery option lists all available tmux sessions in the loom socket
 * and allows the user to attach the terminal to one of them.
 *
 * @param id - The ID of the terminal to recover
 * @param state - The application state
 */
export async function handleRecoverAttachSession(id: string, state: AppState): Promise<void> {
  logger.info("Loading available sessions for terminal", { terminalId: id });

  try {
    // Find terminal by id
    const terminal = state.getTerminal(id);
    if (!terminal) {
      logger.error("Terminal not found", new Error("Terminal not found"), { terminalId: id });
      return;
    }

    const sessions = await invoke<string[]>("list_available_sessions");
    logger.info("Found sessions", { terminalId: id, sessionCount: sessions.length });

    // Note: Recovery UI removed - app now auto-recovers missing sessions
    // Manual recovery handlers are deprecated but kept for compatibility
  } catch (error) {
    logger.error("Failed to list sessions", error, { terminalId: id });
    alert(`Failed to list available sessions: ${error}`);
  }
}

/**
 * Attach a terminal to an existing tmux session
 *
 * This is called when the user selects a session from the available sessions list.
 * It updates the terminal's session attachment and clears the missing session flag.
 *
 * @param terminalId - The ID of the terminal to attach
 * @param sessionName - The name of the tmux session to attach to
 * @param deps - Dependencies (state, saveCurrentConfig)
 */
export async function handleAttachToSession(
  terminalId: string,
  sessionName: string,
  deps: Pick<RecoveryDependencies, "state" | "saveCurrentConfig">
): Promise<void> {
  const { state, saveCurrentConfig } = deps;
  logger.info("Attaching terminal to session", { terminalId, sessionName });

  try {
    await invoke("attach_to_session", {
      id: terminalId,
      sessionName,
    });

    // Update terminal status
    const terminal = state.getTerminals().find((t) => t.id === terminalId);
    if (terminal) {
      state.updateTerminal(terminalId, {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
    }

    // Save config
    await saveCurrentConfig();

    logger.info("Attached successfully", { terminalId, sessionName });
  } catch (error) {
    logger.error("Failed to attach to session", error, { terminalId, sessionName });
    alert(`Failed to attach to session: ${error}`);
  }
}

/**
 * Kill a tmux session and refresh the available sessions list
 *
 * This is used to clean up orphaned tmux sessions that are no longer needed.
 * After killing a session, the available sessions list is refreshed.
 *
 * @param sessionName - The name of the tmux session to kill
 * @param state - The application state (for finding which terminal is showing the session list)
 */
export async function handleKillSession(sessionName: string, _state: AppState): Promise<void> {
  logger.info("Killing session", { sessionName });

  const confirmed = await ask(
    `Are you sure you want to kill session "${sessionName}"?\n\nThis will permanently terminate the session and cannot be undone.`,
    {
      title: "Kill Session",
      type: "warning",
    }
  );

  if (!confirmed) {
    return;
  }

  try {
    await invoke("kill_session", { sessionName });
    logger.info("Session killed successfully", { sessionName });

    // Note: Recovery UI removed - app now auto-recovers missing sessions
    // Manual recovery handlers are deprecated but kept for compatibility
  } catch (error) {
    logger.error("Failed to kill session", error, { sessionName });
    alert(`Failed to kill session: ${error}`);
  }
}
