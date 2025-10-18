/**
 * UI Event Handler Setup Functions
 *
 * Functions for attaching event listeners to UI elements.
 * These are called dynamically when UI is rendered.
 */

import { Logger } from "./logger";
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
