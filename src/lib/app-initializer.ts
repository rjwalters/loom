/**
 * App Initialization
 *
 * Handles application startup including dependency checks and workspace loading.
 * Implements a priority-based workspace loading system:
 * 1. CLI argument (--workspace flag)
 * 2. LocalStorage (HMR survival)
 * 3. Tauri storage (persistent)
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import type { AppState } from "./state";

const logger = Logger.forComponent("app-initializer");

/**
 * Check dependencies on startup
 *
 * Verifies all required system dependencies are installed.
 * Exits the app if dependencies are missing and user declines to retry.
 *
 * @returns True if all dependencies are present, false otherwise
 */
export async function checkDependenciesOnStartup(): Promise<boolean> {
  const { checkAndReportDependencies } = await import("./dependency-checker");
  const hasAllDependencies = await checkAndReportDependencies();

  if (!hasAllDependencies) {
    // User chose not to retry, close the app gracefully
    logger.error("Missing dependencies, exiting", new Error("Missing dependencies"));
    const { exit } = await import("@tauri-apps/plugin-process");
    await exit(1);
    return false;
  }

  return true;
}

/**
 * Attempt to load workspace if valid
 *
 * Validates the workspace path and loads it if valid.
 * Logs appropriate messages with source context for debugging.
 *
 * @param path - Workspace path to validate and load
 * @param source - Source context for logging (e.g., "CLI", "Tauri storage", "localStorage")
 * @param deps - Dependencies (validateWorkspacePath, handleWorkspacePathInput, state)
 * @returns true if workspace was loaded successfully, false if invalid
 */
async function tryLoadWorkspace(
  path: string,
  source: string,
  deps: {
    validateWorkspacePath: (path: string) => Promise<boolean>;
    handleWorkspacePathInput: (path: string) => Promise<void>;
    state: AppState;
  }
): Promise<boolean> {
  const { validateWorkspacePath, handleWorkspacePathInput, state } = deps;

  const isValid = await validateWorkspacePath(path);

  if (isValid) {
    logger.info(`${source} workspace is valid, loading`, { path });
    await handleWorkspacePathInput(path);
    state.setInitializing(false);
    return true;
  }

  logger.info(`${source} workspace is invalid`, { path });
  return false;
}

/**
 * Initialize application
 *
 * Handles app startup sequence:
 * 1. Check dependencies
 * 2. Try CLI workspace argument (highest priority)
 * 3. Try localStorage workspace (HMR survival)
 * 4. Try Tauri storage workspace (persistent)
 * 5. Show workspace picker if no valid workspace found
 *
 * @param state - Application state
 * @param validateWorkspacePath - Function to validate workspace paths
 * @param handleWorkspacePathInput - Function to handle workspace path input
 * @param render - Function to render the UI
 */
export async function initializeApp(deps: {
  state: AppState;
  validateWorkspacePath: (path: string) => Promise<boolean>;
  handleWorkspacePathInput: (path: string) => Promise<void>;
  render: () => void;
}): Promise<void> {
  const { state, validateWorkspacePath, handleWorkspacePathInput, render } = deps;

  // Set initializing state to show loading UI
  state.setInitializing(true);
  logger.info("Starting initialization");

  // Check dependencies first
  const hasAllDependencies = await checkDependenciesOnStartup();
  if (!hasAllDependencies) {
    state.setInitializing(false);
    return; // Exit early if dependencies are missing
  }

  // PRIORITY 1: Check for CLI workspace argument (highest priority)
  try {
    const cliWorkspace = await invoke<string | null>("get_cli_workspace");
    if (cliWorkspace) {
      logger.info("Found CLI workspace argument", {
        cliWorkspace,
        priority: "highest",
      });
      logger.info("Using CLI workspace (takes precedence over stored workspace)");

      // Validate and load CLI workspace
      if (
        await tryLoadWorkspace(cliWorkspace, "CLI", {
          validateWorkspacePath,
          handleWorkspacePathInput,
          state,
        })
      ) {
        return; // CLI workspace loaded successfully - skip stored workspace
      }

      logger.warn("CLI workspace is invalid, falling back to stored workspace", {
        cliWorkspace,
      });
    } else {
      logger.info("No CLI workspace argument provided");
    }
  } catch (error) {
    logger.error("Failed to get CLI workspace", error);
    // Continue to stored workspace fallback
  }

  // PRIORITY 2: Try to restore workspace from localStorage (for HMR survival)
  const localStorageWorkspace = state.workspace.restoreWorkspaceFromLocalStorage();
  if (localStorageWorkspace) {
    logger.info("Restored workspace from localStorage (HMR survival)", {
      localStorageWorkspace,
    });
    logger.info("This prevents HMR from clearing the workspace during hot reload");
  }

  try {
    // PRIORITY 3: Check for stored workspace in Tauri storage (lowest priority)
    const storedPath = await invoke<string | null>("get_stored_workspace");

    if (storedPath) {
      logger.info("Found stored workspace", { storedPath, priority: "lowest" });

      // Validate and load stored workspace
      if (
        await tryLoadWorkspace(storedPath, "Tauri storage", {
          validateWorkspacePath,
          handleWorkspacePathInput,
          state,
        })
      ) {
        return;
      }

      // Path no longer valid - clear it and show picker
      logger.info("Stored workspace invalid, clearing", { storedPath });
      await invoke("clear_stored_workspace");
      localStorage.removeItem("loom:workspace"); // Also clear localStorage
    } else if (localStorageWorkspace) {
      // No Tauri storage but have localStorage (HMR case)
      logger.info("Using localStorage workspace after HMR", {
        localStorageWorkspace,
      });

      if (
        await tryLoadWorkspace(localStorageWorkspace, "localStorage", {
          validateWorkspacePath,
          handleWorkspacePathInput,
          state,
        })
      ) {
        return;
      }

      // Invalid - clear it
      logger.info("localStorage workspace is invalid, clearing", {
        localStorageWorkspace,
      });
      localStorage.removeItem("loom:workspace");
    }
  } catch (error) {
    logger.error("Failed to load stored workspace", error);

    // If Tauri storage failed but we have localStorage, try that
    if (localStorageWorkspace) {
      logger.info("Tauri storage failed, trying localStorage workspace", {
        localStorageWorkspace,
      });

      if (
        await tryLoadWorkspace(localStorageWorkspace, "localStorage (fallback)", {
          validateWorkspacePath,
          handleWorkspacePathInput,
          state,
        })
      ) {
        return;
      }

      logger.info("localStorage workspace is invalid", { localStorageWorkspace });
    }
  }

  // No workspace found or all validation failed - show picker
  logger.info("No valid workspace found, showing workspace picker");
  state.setInitializing(false);
  render();
}
