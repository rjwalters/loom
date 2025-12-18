import { saveCurrentConfiguration } from "./config";
import { Logger } from "./logger";
import { createTerminalsWithRetry, type TerminalConfig } from "./parallel-terminal-creator";
import type { AppState, Terminal } from "./state";
import { showToast } from "./toast";

const logger = Logger.forComponent("workspace-common");

/**
 * Workspace terminal initialization shared logic
 *
 * This module provides common functionality used by both workspace-lifecycle.ts
 * (for new workspace initialization) and workspace-start.ts (for starting the engine).
 *
 * The shared logic includes:
 * - Converting agent configs to terminal configs
 * - Creating terminal sessions in parallel with retry
 * - Updating agent IDs from created terminal sessions
 * - Loading terminals into app state
 * - Launching agents for terminals
 * - Saving configuration
 */

/**
 * Options for initializing terminals
 *
 * Provides flexibility to handle differences between new workspace initialization
 * and existing workspace engine start.
 */
export interface InitializeTerminalsOptions {
  /** Path to the workspace directory */
  workspacePath: string;
  /** Array of agents/terminals from loaded config */
  agents: Terminal[];
  /** Application state instance */
  state: AppState;
  /** Function to launch agents for terminals with role configs */
  launchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
  /**
   * Clear worktree paths on created terminals (workspace-start behavior).
   * When true, sets agent.worktreePath = "" for all created terminals.
   * @default false
   */
  clearWorktreePaths?: boolean;
  /**
   * Set workspace as active before loading terminals (workspace-start behavior).
   * When true, calls state.workspace.setWorkspace(workspacePath) before loadTerminals.
   * @default false
   */
  setWorkspaceActive?: boolean;
  /**
   * Save configuration before launching agents (workspace-start behavior).
   * When true, calls saveCurrentConfiguration after loadTerminals but before launch.
   * @default false
   */
  saveBeforeLaunch?: boolean;
  /**
   * Show toast for each terminal creation failure (workspace-lifecycle behavior).
   * When true, shows a toast per failure; when false, shows combined failure toast.
   * @default true
   */
  toastPerFailure?: boolean;
  /** Optional prefix for log messages */
  logPrefix?: string;
}

/**
 * Result of terminal initialization
 */
export interface InitializeTerminalsResult {
  /** Number of terminals created successfully */
  succeededCount: number;
  /** Number of terminals that failed to create */
  failedCount: number;
}

/**
 * Initialize terminals for a workspace
 *
 * This function handles the common terminal initialization flow shared between
 * new workspace setup and existing workspace engine start:
 *
 * 1. Convert agents to terminal configs
 * 2. Create terminal sessions in parallel with retry
 * 3. Update agent IDs from created sessions
 * 4. Optionally set workspace as active
 * 5. Load terminals into app state
 * 6. Optionally save config before launch
 * 7. Launch agents for terminals with role configs
 * 8. Save final configuration
 *
 * @param options - Initialization options
 * @returns Result with succeeded and failed counts
 */
export async function initializeTerminals(
  options: InitializeTerminalsOptions
): Promise<InitializeTerminalsResult> {
  const {
    workspacePath,
    agents,
    state,
    launchAgentsForTerminals,
    clearWorktreePaths = false,
    setWorkspaceActive = false,
    saveBeforeLaunch = false,
    toastPerFailure = true,
    logPrefix = "workspace-common",
  } = options;

  logger.info("Initializing terminals", {
    workspacePath,
    terminalCount: agents.length,
    source: logPrefix,
  });

  // Convert agents to terminal configs for parallel creation
  const terminalConfigs: TerminalConfig[] = agents.map((agent) => ({
    id: agent.id,
    name: agent.name,
    role: agent.role || "default",
    workingDir: workspacePath,
    instanceNumber: 0, // Will be assigned by createTerminalsWithRetry
  }));

  // Create terminal sessions in parallel with retry logic
  const { succeeded, failed } = await createTerminalsWithRetry(
    terminalConfigs,
    workspacePath,
    state
  );

  // Update agent IDs for successfully created terminals
  for (const result of succeeded) {
    const agent = agents.find((a) => a.id === result.configId);
    if (agent) {
      agent.id = result.terminalId;

      // Optionally clear worktree paths (workspace-start behavior)
      if (clearWorktreePaths) {
        agent.worktreePath = "";
      }

      logger.info("Created terminal", {
        terminalName: agent.name,
        terminalId: result.terminalId,
        workspacePath,
        source: logPrefix,
      });
    }
  }

  // Handle failures
  if (failed.length > 0) {
    if (toastPerFailure) {
      // workspace-lifecycle behavior: toast per failure
      for (const failure of failed) {
        const agent = agents.find((a) => a.id === failure.configId);
        logger.error("Failed to create terminal", failure.error as Error, {
          terminalName: agent?.name,
          workspacePath,
          source: logPrefix,
        });
        showToast(`Failed to create terminal ${agent?.name}: ${failure.error}`, "error");
      }
    } else {
      // workspace-start behavior: combined toast
      const failedNames = failed
        .map((f) => {
          const agent = agents.find((a) => a.id === f.configId);
          return agent?.name || f.configId;
        })
        .join(", ");

      logger.error(
        "Some terminals failed to create after retries",
        new Error("Terminal creation failures"),
        {
          workspacePath,
          failedCount: failed.length,
          failedNames,
          source: logPrefix,
        }
      );

      showToast(
        `Failed to create ${failed.length} terminal(s) after retries: ${failedNames}. Successfully created ${succeeded.length} of ${agents.length} terminals.`,
        "error",
        7000
      );
    }
  }

  logger.info("Terminal creation complete", {
    workspacePath,
    totalTerminals: agents.length,
    succeeded: succeeded.length,
    failed: failed.length,
    agents: agents.map((a) => `${a.name}=${a.id}`),
    source: logPrefix,
  });

  // Optionally set workspace as active before loading terminals
  if (setWorkspaceActive) {
    state.workspace.setWorkspace(workspacePath);
  }

  // Load agents into state with their new IDs
  logger.info("Loading agents into state", {
    workspacePath,
    agents: agents.map((a) => `${a.name}=${a.id}`),
    source: logPrefix,
  });
  state.terminals.loadTerminals(agents);
  logger.info("State after loadTerminals", {
    workspacePath,
    terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
    source: logPrefix,
  });

  // Optionally save config before launching agents
  if (saveBeforeLaunch) {
    logger.info("Saving config with real terminal IDs (before launch)", {
      workspacePath,
      source: logPrefix,
    });
    await saveCurrentConfiguration(state);
  }

  // Launch agents for terminals with role configs
  logger.info("Launching agents", {
    workspacePath,
    source: logPrefix,
  });
  await launchAgentsForTerminals(workspacePath, agents);
  logger.info("State after launchAgentsForTerminals", {
    workspacePath,
    terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
    source: logPrefix,
  });

  // Save the updated config with real terminal IDs
  logger.info("Saving config with real terminal IDs", {
    workspacePath,
    source: logPrefix,
  });
  await saveCurrentConfiguration(state);

  return {
    succeededCount: succeeded.length,
    failedCount: failed.length,
  };
}
