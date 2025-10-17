/**
 * UI Event Handler Setup Functions
 *
 * Functions for attaching event listeners to UI elements.
 * These are called dynamically when UI is rendered.
 */

import { getTooltipManager } from "./tooltip";

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
  console.log("[attachWorkspaceEventListeners] attaching listeners...");
  // Workspace path input - validate on Enter or blur
  const workspaceInput = document.getElementById("workspace-path") as HTMLInputElement;
  console.log("[attachWorkspaceEventListeners] workspaceInput:", workspaceInput);
  if (workspaceInput) {
    workspaceInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        console.log("[workspaceInput keydown] Enter pressed, value:", workspaceInput.value);
        e.preventDefault();
        handleWorkspacePathInput(workspaceInput.value);
        workspaceInput.blur();
      }
    });

    workspaceInput.addEventListener("blur", () => {
      console.log(
        "[workspaceInput blur] value:",
        workspaceInput.value,
        "workspace:",
        getCurrentWorkspace()
      );
      if (workspaceInput.value !== getCurrentWorkspace()) {
        handleWorkspacePathInput(workspaceInput.value);
      }
    });
  }

  // Browse workspace button
  const browseBtn = document.getElementById("browse-workspace");
  console.log("[attachWorkspaceEventListeners] browseBtn:", browseBtn);
  browseBtn?.addEventListener("click", () => {
    console.log("[browseBtn click] clicked");
    browseWorkspace();
  });

  // Create new project button
  const createProjectBtn = document.getElementById("create-new-project-btn");
  console.log("[attachWorkspaceEventListeners] createProjectBtn:", createProjectBtn);
  createProjectBtn?.addEventListener("click", async () => {
    console.log("[createProjectBtn click] clicked");
    const { showCreateProjectModal } = await import("./create-project-modal");
    showCreateProjectModal(async (projectPath: string) => {
      console.log(`[createProjectModal] Project created at: ${projectPath}`);
      // Load the newly created project as the workspace
      await handleWorkspacePathInput(projectPath);
    });
  });
}
