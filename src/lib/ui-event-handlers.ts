/**
 * UI Event Handler Setup Functions
 *
 * Functions for attaching event listeners to UI elements.
 * These are called dynamically when UI is rendered.
 */

import { invoke } from "@tauri-apps/api/core";
import type { AppLevelState } from "./app-state";
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
  }) => Promise<Terminal | undefined>;
  generateNextConfigId: (terminals: Terminal[]) => string;
}): void {
  const {
    state,
    render,
    saveCurrentConfig,
    terminalManager,
    outputPoller,
    healthMonitor,
    appLevelState,
    // Note: createPlainTerminal and generateNextConfigId
    // are unused after Phase 2 (side-by-side layout) removed mini terminal row
  } = deps;

  // Theme toggle
  document.getElementById("theme-toggle")?.addEventListener("click", () => {
    toggleTheme();
  });

  // Close workspace button
  document.getElementById("close-workspace-btn")?.addEventListener("click", async () => {
    if (state.workspace.hasWorkspace()) {
      await invoke("emit_event", { event: "close-workspace" });
    }
  });

  // Terminal view - double-click to rename, click for settings/clear
  const terminalView = document.getElementById("terminal-view");
  if (terminalView) {
    // Button clicks (settings, clear)
    terminalView.addEventListener("click", async (e) => {
      const target = e.target as HTMLElement;

      // Settings button
      const settingsBtn = target.closest("#terminal-settings-btn");
      if (settingsBtn) {
        e.stopPropagation();
        const id = settingsBtn.getAttribute("data-terminal-id");
        if (id) {
          logger.info("Opening terminal settings", { terminalId: id });
          const terminal = state.terminals.getTerminals().find((t) => t.id === id);
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

      // Search button
      const searchBtn = target.closest("#terminal-search-btn");
      if (searchBtn) {
        e.stopPropagation();
        const searchPanel = document.getElementById("terminal-search-panel");
        const searchInput = document.getElementById("terminal-search-input") as HTMLInputElement;
        if (searchPanel && searchInput) {
          searchPanel.classList.remove("hidden");
          searchInput.focus();
          searchInput.select();
        }
        return;
      }

      // Export button
      const exportBtn = target.closest("#terminal-export-btn");
      if (exportBtn) {
        e.stopPropagation();
        const id = exportBtn.getAttribute("data-terminal-id");
        if (id) {
          logger.info("Exporting terminal", { terminalId: id });
          terminalManager
            .exportAndDownload(id)
            .then(() => {
              logger.info("Terminal exported successfully", { terminalId: id });
            })
            .catch((error) => {
              logger.error("Failed to export terminal", error, { terminalId: id });
            });
        }
        return;
      }

      // Search panel - Close button
      const searchCloseBtn = target.closest("#terminal-search-close");
      if (searchCloseBtn) {
        e.stopPropagation();
        const searchPanel = document.getElementById("terminal-search-panel");
        if (searchPanel) {
          searchPanel.classList.add("hidden");
          const terminal = state.terminals.getPrimary();
          if (terminal) {
            terminalManager.clearSearch(terminal.id);
          }
        }
        return;
      }

      // Search panel - Next button
      const searchNextBtn = target.closest("#terminal-search-next");
      if (searchNextBtn) {
        e.stopPropagation();
        const terminal = state.terminals.getPrimary();
        if (terminal) {
          terminalManager.findNext(terminal.id);
        }
        return;
      }

      // Search panel - Previous button
      const searchPrevBtn = target.closest("#terminal-search-prev");
      if (searchPrevBtn) {
        e.stopPropagation();
        const terminal = state.terminals.getPrimary();
        if (terminal) {
          terminalManager.findPrevious(terminal.id);
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
    terminalView.addEventListener("dblclick", (e) => {
      const target = e.target as HTMLElement;

      if (target.classList.contains("terminal-name")) {
        e.stopPropagation();
        const id = target.getAttribute("data-terminal-id");
        if (id) {
          startRename(id, target, { state, saveCurrentConfig, render });
        }
      }
    });

    // Search input - handle input changes
    terminalView.addEventListener("input", (e) => {
      const target = e.target as HTMLElement;
      if (target.id === "terminal-search-input") {
        const input = target as HTMLInputElement;
        const caseSensitive =
          (document.getElementById("terminal-search-case-sensitive") as HTMLInputElement)
            ?.checked || false;
        const regex =
          (document.getElementById("terminal-search-regex") as HTMLInputElement)?.checked || false;
        const terminal = state.terminals.getPrimary();

        if (terminal && input.value) {
          terminalManager.searchTerminal(terminal.id, input.value, {
            caseSensitive,
            regex,
          });
        } else if (terminal) {
          terminalManager.clearSearch(terminal.id);
        }
      }
    });

    // Search options - handle checkbox changes
    terminalView.addEventListener("change", (e) => {
      const target = e.target as HTMLElement;
      if (target.id === "terminal-search-case-sensitive" || target.id === "terminal-search-regex") {
        // Re-run search with updated options
        const input = document.getElementById("terminal-search-input") as HTMLInputElement;
        const caseSensitive =
          (document.getElementById("terminal-search-case-sensitive") as HTMLInputElement)
            ?.checked || false;
        const regex =
          (document.getElementById("terminal-search-regex") as HTMLInputElement)?.checked || false;
        const terminal = state.terminals.getPrimary();

        if (terminal && input && input.value) {
          terminalManager.searchTerminal(terminal.id, input.value, {
            caseSensitive,
            regex,
          });
        }
      }
    });

    // Search input - handle keyboard shortcuts
    terminalView.addEventListener("keydown", (e) => {
      const target = e.target as HTMLElement;

      // Search input shortcuts
      if (target.id === "terminal-search-input") {
        const terminal = state.terminals.getPrimary();

        if (e.key === "Enter" && terminal) {
          e.preventDefault();
          if (e.shiftKey) {
            // Shift+Enter: Previous match
            terminalManager.findPrevious(terminal.id);
          } else {
            // Enter: Next match
            terminalManager.findNext(terminal.id);
          }
        } else if (e.key === "Escape") {
          // Esc: Close search panel
          e.preventDefault();
          const searchPanel = document.getElementById("terminal-search-panel");
          if (searchPanel) {
            searchPanel.classList.add("hidden");
            if (terminal) {
              terminalManager.clearSearch(terminal.id);
            }
          }
        }
      }
    });

    // Global keyboard shortcut: Cmd+F to open search
    terminalView.addEventListener("keydown", (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "f") {
        e.preventDefault();
        const searchPanel = document.getElementById("terminal-search-panel");
        const searchInput = document.getElementById("terminal-search-input") as HTMLInputElement;
        if (searchPanel && searchInput) {
          searchPanel.classList.remove("hidden");
          searchInput.focus();
          searchInput.select();
        }
      }
    });
  }

  // Note: Mini terminal row removed in Phase 2 (side-by-side layout)
  // Terminal management now handled through terminal-view and analytics-view panels
}
