/**
 * Terminal Action Handlers
 *
 * Functions for handling terminal-specific UI actions like running interval prompts
 * and inline renaming. These are called from event handlers in main.ts.
 */

import { Logger } from "./logger";
import type { AppState } from "./state";

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

    // Import autonomous manager
    const { getAutonomousManager } = await import("./autonomous-manager");
    const autonomousManager = getAutonomousManager();

    // Execute the interval prompt and reset timer
    await autonomousManager.runNow(terminal);
    logger.info("Successfully executed interval prompt", { terminalId });
  } catch (error) {
    logger.error("Failed to execute interval prompt", error as Error, {
      terminalId,
    });
    alert(`Failed to run interval prompt: ${error}`);
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
  }, 0);

  const commit = () => {
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
