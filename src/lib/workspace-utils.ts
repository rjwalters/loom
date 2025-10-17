/**
 * Workspace Utility Functions
 *
 * Pure utility functions for workspace path handling and validation UI.
 * These functions have no state dependencies and can be safely used anywhere.
 */

import { homeDir } from "@tauri-apps/api/path";

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
      console.error("Failed to get home directory:", error);
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
  console.log("[showWorkspaceError]", message);
  const input = document.getElementById("workspace-path") as HTMLInputElement;
  const errorDiv = document.getElementById("workspace-error");

  console.log("[showWorkspaceError] input:", input, "errorDiv:", errorDiv);

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
  console.log("[clearWorkspaceError]");
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
