/**
 * Workspace Utility Functions
 *
 * Pure utility functions for workspace path handling and validation UI.
 * These functions have no state dependencies and can be safely used anywhere.
 */

import { open } from "@tauri-apps/api/dialog";
import { homeDir } from "@tauri-apps/api/path";
import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";
import type { Terminal } from "./state";
import { showToast } from "./toast";

const logger = Logger.forComponent("workspace-utils");

/**
 * Expand tilde (~) to home directory in file paths
 *
 * @param path - The path that may contain a tilde
 * @returns The expanded path with home directory, or original path if no tilde
 *
 * @example
 * await expandTildePath("~/Documents") // "/Users/username/Documents"
 * await expandTildePath("/absolute/path") // "/absolute/path"
 */
export async function expandTildePath(path: string): Promise<string> {
  if (path.startsWith("~")) {
    try {
      const home = await homeDir();
      return path.replace(/^~/, home);
    } catch (error) {
      logger.error("Failed to get home directory", error as Error, { path });
      return path;
    }
  }
  return path;
}

/**
 * Show workspace validation error in the UI
 *
 * Highlights the workspace input field and displays an error message.
 * This provides visual feedback when workspace validation fails.
 *
 * @param message - The error message to display
 */
export function showWorkspaceError(message: string): void {
  logger.info("Showing workspace error", { message });
  const input = document.getElementById("workspace-path") as HTMLInputElement;
  const errorDiv = document.getElementById("workspace-error");

  logger.info("Found workspace error UI elements", {
    hasInput: !!input,
    hasErrorDiv: !!errorDiv,
  });

  if (input) {
    input.classList.remove("border-gray-300", "dark:border-gray-600");
    input.classList.add("border-red-500", "dark:border-red-500");
  }

  if (errorDiv) {
    errorDiv.textContent = message;
  }
}

/**
 * Clear workspace validation error from the UI
 *
 * Removes error highlighting from the workspace input field
 * and clears the error message display.
 */
export function clearWorkspaceError(): void {
  logger.info("Clearing workspace error");
  const input = document.getElementById("workspace-path") as HTMLInputElement;
  const errorDiv = document.getElementById("workspace-error");

  if (input) {
    input.classList.remove("border-red-500", "dark:border-red-500");
    input.classList.add("border-gray-300", "dark:border-gray-600");
  }

  if (errorDiv) {
    errorDiv.textContent = "";
  }
}

/**
 * Validate workspace path
 *
 * Checks if the provided path is a valid git repository using Tauri IPC.
 * Updates the workspace error UI based on validation result.
 *
 * @param path - The workspace path to validate
 * @returns True if valid git repository, false otherwise
 */
export async function validateWorkspacePath(path: string): Promise<boolean> {
  logger.info("Validating workspace path", { path });
  if (!path || path.trim() === "") {
    logger.info("Empty path, clearing error");
    clearWorkspaceError();
    return false;
  }

  try {
    await invoke<boolean>("validate_git_repo", { path });
    logger.info("Workspace validation passed", { path });
    clearWorkspaceError();
    return true;
  } catch (error) {
    const errorMessage =
      typeof error === "string"
        ? error
        : (error as { message?: string })?.message || "Invalid workspace path";
    logger.warn("Workspace validation failed", { path, errorMessage });
    showWorkspaceError(errorMessage);
    return false;
  }
}

/**
 * Browse for workspace folder
 *
 * Opens a native folder picker dialog and handles the selected path.
 *
 * @param handleWorkspacePathInput - Callback to handle the selected path
 */
export async function browseWorkspace(
  handleWorkspacePathInput: (path: string) => Promise<void>
): Promise<void> {
  try {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "Select workspace folder",
    });

    if (selected && typeof selected === "string") {
      await handleWorkspacePathInput(selected);
    }
  } catch (error) {
    logger.error("Error selecting workspace", error);
    showToast("Failed to select workspace. Please try again.", "error");
  }
}

/**
 * Generate next available config ID
 *
 * Finds the next available terminal-N ID by checking existing terminal IDs.
 *
 * @param terminals - Current list of terminals
 * @returns Next available terminal ID (e.g., "terminal-1", "terminal-2")
 */
export function generateNextConfigId(terminals: Terminal[]): string {
  const existingIds = new Set(terminals.map((t) => t.id));

  // Find the next available terminal-N ID
  let i = 1;
  while (existingIds.has(`terminal-${i}`)) {
    i++;
  }

  return `terminal-${i}`;
}
