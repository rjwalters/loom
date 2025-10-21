/**
 * UI Event Handler Setup Functions
 *
 * Functions for attaching event listeners to UI elements.
 * These are called dynamically when UI is rendered.
 */

import { invoke } from "@tauri-apps/api/tauri";
import type { AppLevelState } from "./app-state";
import type { setupDragAndDrop } from "./drag-drop-manager";
import type { HealthMonitor } from "./health-monitor";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import {
  closeTerminalWithConfirmation,
  handleRestartTerminal,
  handleRunNowClick,
  startRename,
} from "./terminal-actions";
import { showTerminalActivityModal } from "./terminal-activity-modal";
import type { TerminalManager } from "./terminal-manager";
import { showTerminalSettingsModal } from "./terminal-settings-modal";
import { toggleTheme } from "./theme";
import { getTooltipManager } from "./tooltip";

const logger = Logger.forComponent("ui-event-handlers");

/**
 * Set up tooltips for all elements with data-tooltip attributes
 */
export function setupTooltips(): void {
  const tooltipManager = getTooltipManager();

  // Find all elements with data-tooltip attribute
  const elements = document.querySelectorAll<HTMLElement>("[data-tooltip]");

  elements.forEach((element) => {
    const text = element.getAttribute("data-tooltip");
    const position = element.getAttribute("data-tooltip-position") as
      | "top"
      | "bottom"
      | "left"
      | "right"
      | "auto"
      | null;

    if (text) {
      tooltipManager.attach(element, {
        text,
        position: position || "auto",
        delay: 500,
      });
    }
  });
}

/**
 * Attach workspace event listeners
 *
 * Called dynamically when workspace selector is rendered.
 * Sets up event handlers for workspace input, browse button, and create project button.
 *
 * @param handleWorkspacePathInput - Callback for handling workspace path input
 * @param browseWorkspace - Callback for browsing workspace folder
 * @param getCurrentWorkspace - Callback to get current workspace path (for blur validation check)
 */
export function attachWorkspaceEventListeners(
  handleWorkspacePathInput: (path: string) => void,
  browseWorkspace: () => void,
  getCurrentWorkspace: () => string
): void {
  logger.info("Attaching workspace event listeners");
  // Workspace path input - validate on Enter or blur
  const workspaceInput = document.getElementById("workspace-path") as HTMLInputElement;
  logger.info("Found workspace input element", {
    hasElement: !!workspaceInput,
  });
  if (workspaceInput) {
    workspaceInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        logger.info("Enter key pressed in workspace input", {
          value: workspaceInput.value,
        });
        e.preventDefault();
        handleWorkspacePathInput(workspaceInput.value);
        workspaceInput.blur();
      }
    });

    workspaceInput.addEventListener("blur", () => {
      logger.info("Workspace input blur event", {
        inputValue: workspaceInput.value,
        currentWorkspace: getCurrentWorkspace(),
      });
      if (workspaceInput.value !== getCurrentWorkspace()) {
        handleWorkspacePathInput(workspaceInput.value);
      }
    });
  }

  // Browse workspace button
  const browseBtn = document.getElementById("browse-workspace");
  logger.info("Found browse button element", {
    hasElement: !!browseBtn,
  });
  browseBtn?.addEventListener("click", () => {
    logger.info("Browse workspace button clicked");
    browseWorkspace();
  });

  // Create new project button
  const createProjectBtn = document.getElementById("create-new-project-btn");
  logger.info("Found create project button element", {
    hasElement: !!createProjectBtn,
  });
  createProjectBtn?.addEventListener("click", async () => {
    logger.info("Create project button clicked");
    const { showCreateProjectModal } = await import("./create-project-modal");
    showCreateProjectModal(async (projectPath: string) => {
      logger.info("Project created via modal", { projectPath });
      // Load the newly created project as the workspace
      await handleWorkspacePathInput(projectPath);
    });
  });
}

/**
 * Set up all main application event listeners
 *
 * Consolidates event handling for theme toggle, workspace controls,
 * primary terminal interactions, and mini terminal row interactions.
 *
 * @param deps - Dependencies required for event handlers
 */
export function setupMainEventListeners(deps: {
  state: AppState;
  render: () => void;
  saveCurrentConfig: () => Promise<void>;
  terminalManager: TerminalManager;
  outputPoller: OutputPoller;
  healthMonitor: HealthMonitor;
  appLevelState: AppLevelState;
  createPlainTerminal: (deps: {
    state: AppState;
    workspacePath: string;
    generateNextConfigId: (terminals: Terminal[]) => string;
    saveCurrentConfig: () => Promise<void>;
  }) => Promise<void>;
  generateNextConfigId: (terminals: Terminal[]) => string;
  setupDragAndDrop: typeof setupDragAndDrop;
}): void {
  const {
    state,
    render,
    saveCurrentConfig,
    terminalManager,
    outputPoller,
    healthMonitor,
    appLevelState,
    createPlainTerminal,
    generateNextConfigId,
    setupDragAndDrop,
  } = deps;

  // Theme toggle
  document.getElementById("theme-toggle")?.addEventListener("click", () => {
    toggleTheme();
  });

  // Close workspace button
  document.getElementById("close-workspace-btn")?.addEventListener("click", async () => {
    if (state.hasWorkspace()) {
      await invoke("emit_event", { event: "close-workspace" });
    }
  });

  // Primary terminal - double-click to rename, click for settings/clear
  const primaryTerminal = document.getElementById("primary-terminal");
  if (primaryTerminal) {
    // Button clicks (settings, clear)
    primaryTerminal.addEventListener("click", async (e) => {
      const target = e.target as HTMLElement;

      // Settings button
      const settingsBtn = target.closest("#terminal-settings-btn");
      if (settingsBtn) {
        e.stopPropagation();
        const id = settingsBtn.getAttribute("data-terminal-id");
        if (id) {
          logger.info("Opening terminal settings", { terminalId: id });
          const terminal = state.getTerminals().find((t) => t.id === id);
          if (terminal) {
            showTerminalSettingsModal(terminal, state, render);
          }
        }
        return;
      }

      // Clear button
      const clearBtn = target.closest("#terminal-clear-btn");
      if (clearBtn) {
        e.stopPropagation();
        const id = clearBtn.getAttribute("data-terminal-id");
        if (id) {
          logger.info("Clearing terminal", { terminalId: id });
          terminalManager.clearTerminal(id);
        }
        return;
      }

      // Restart Terminal button
      const restartBtn = target.closest(".restart-terminal-btn");
      if (restartBtn) {
        e.stopPropagation();
        const id = restartBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRestartTerminal(id, { state, saveCurrentConfig });
        }
        return;
      }

      // Run Now button (interval mode)
      const runNowBtn = target.closest(".run-now-btn");
      if (runNowBtn) {
        e.stopPropagation();
        const id = runNowBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRunNowClick(id, { state });
        }
        return;
      }

      // Close button
      const closeBtn = target.closest("#terminal-close-btn");
      if (closeBtn) {
        e.stopPropagation();
        const id = closeBtn.getAttribute("data-terminal-id");
        if (id) {
          await closeTerminalWithConfirmation(id, {
            state,
            outputPoller,
            terminalManager,
            appLevelState,
            saveCurrentConfig,
          });
        }
        return;
      }

      // Health Check - Check Now button
      const checkNowBtn = target.closest("#check-now-btn");
      if (checkNowBtn) {
        e.stopPropagation();
        const id = checkNowBtn.getAttribute("data-terminal-id");
        if (id) {
          logger.info("Triggering immediate health check", { terminalId: id });
          // Trigger immediate health check
          healthMonitor
            .performHealthCheck()
            .then(() => {
              logger.info("Health check complete", { terminalId: id });
            })
            .catch((error: unknown) => {
              logger.error("Health check failed", error, { terminalId: id });
            });
        }
        return;
      }
    });

    // Double-click to rename
    primaryTerminal.addEventListener("dblclick", (e) => {
      const target = e.target as HTMLElement;

      if (target.classList.contains("terminal-name")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");
        if (id) {
          startRename(id, target, { state, saveCurrentConfig, render });
        }
      }
    });
  }

  // Mini terminal row - event delegation for dynamic children
  const miniRow = document.getElementById("mini-terminal-row");
  if (miniRow) {
    miniRow.addEventListener("click", async (e) => {
      const target = e.target as HTMLElement;

      // Handle Restart Terminal button clicks
      const restartBtn = target.closest(".restart-terminal-btn");
      if (restartBtn) {
        e.stopPropagation();
        const id = restartBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRestartTerminal(id, { state, saveCurrentConfig });
        }
        return;
      }

      // Handle Run Now button clicks (interval mode)
      const runNowBtn = target.closest(".run-now-btn");
      if (runNowBtn) {
        e.stopPropagation();
        const id = runNowBtn.getAttribute("data-terminal-id");
        if (id) {
          handleRunNowClick(id, { state });
        }
        return;
      }

      // Handle Show Activity button clicks
      const activityBtn = target.closest(".show-activity-btn");
      if (activityBtn) {
        e.stopPropagation();
        const id = activityBtn.getAttribute("data-terminal-id");
        if (id) {
          const terminal = state.getTerminal(id);
          if (terminal) {
            showTerminalActivityModal(id, terminal.name);
          }
        }
        return;
      }

      // Handle close button clicks
      if (target.classList.contains("close-terminal-btn")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");

        if (id) {
          await closeTerminalWithConfirmation(id, {
            state,
            outputPoller,
            terminalManager,
            appLevelState,
            saveCurrentConfig,
          });
        }
        return;
      }

      // Handle add terminal button
      if (target.id === "add-terminal-btn" || target.closest("#add-terminal-btn")) {
        // Don't add if no workspace selected
        if (!state.hasWorkspace()) {
          return;
        }

        // Create plain terminal
        createPlainTerminal({
          state,
          workspacePath: state.getWorkspaceOrThrow(),
          generateNextConfigId,
          saveCurrentConfig,
        });
        return;
      }

      // Handle terminal card clicks (switch primary)
      const card = target.closest("[data-terminal-id]");
      if (card) {
        const id = card.getAttribute("data-terminal-id");
        if (id) {
          state.setPrimary(id);
        }
      }
    });

    // Handle mousedown to show immediate visual feedback
    miniRow.addEventListener("mousedown", (e) => {
      const target = e.target as HTMLElement;

      // Don't handle if clicking close button
      if (target.classList.contains("close-terminal-btn")) {
        return;
      }

      const card = target.closest(".terminal-card");
      if (card) {
        // Remove selection from all cards and restore default border
        document.querySelectorAll(".terminal-card").forEach((c) => {
          c.classList.remove("border-2", "border-blue-500");
          c.classList.add("border", "border-gray-200", "dark:border-gray-700");
        });

        // Add selection to clicked card immediately
        card.classList.remove("border", "border-gray-200", "dark:border-gray-700");
        card.classList.add("border-2", "border-blue-500");
      }
    });

    // Handle double-click to rename terminals
    miniRow.addEventListener("dblclick", (e) => {
      const target = e.target as HTMLElement;

      // Check if double-clicking on the terminal name in mini cards
      if (target.classList.contains("terminal-name")) {
        e.stopPropagation();
        const card = target.closest("[data-terminal-id]");
        const id = card?.getAttribute("data-terminal-id");
        if (id) {
          startRename(id, target, { state, saveCurrentConfig, render });
        }
      }
    });

    // Set up drag-and-drop handlers (extracted to drag-drop-manager.ts)
    setupDragAndDrop(miniRow, state, saveCurrentConfig);
  }
}
