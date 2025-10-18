import { invoke } from "@tauri-apps/api/tauri";
import {
  loadWorkspaceConfig,
  saveConfig,
  saveState,
  setConfigWorkspace,
  splitTerminals,
} from "./config";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { cleanupWorkspace } from "./workspace-cleanup";

const logger = Logger.forComponent("workspace-reset");

/**
 * Dependencies needed by the workspace reset logic
 */
export interface WorkspaceResetDependencies {
  state: AppState;
  outputPoller: OutputPoller;
  terminalManager: TerminalManager;
  setCurrentAttachedTerminalId: (id: string | null) => void;
  launchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
  render: () => void;
}

/**
 * Reset workspace to defaults with optional confirmation
 *
 * This function:
 * 1. Stops all polling and destroys xterm instances
 * 2. Destroys all terminal sessions
 * 3. Kills all loom tmux sessions
 * 4. Resets workspace files to defaults
 * 5. Resets GitHub labels
 * 6. Creates new terminals from config
 * 7. Launches agents for terminals with role configs
 * 8. Saves config and re-renders
 *
 * @param workspacePath - The workspace directory path
 * @param dependencies - Dependencies (state, managers, callbacks)
 * @param logPrefix - Prefix for console.log messages (e.g., "start-workspace" or "force-start-workspace")
 */
export async function resetWorkspaceToDefaults(
  workspacePath: string,
  dependencies: WorkspaceResetDependencies,
  logPrefix: string
): Promise<void> {
  const {
    state,
    outputPoller,
    terminalManager,
    setCurrentAttachedTerminalId,
    launchAgentsForTerminals,
    render,
  } = dependencies;

  logger.info("Resetting workspace to defaults", {
    workspacePath,
    source: logPrefix
  });

  // Cleanup existing terminals and sessions
  await cleanupWorkspace({
    component: logPrefix,
    state,
    outputPoller,
    terminalManager,
    setCurrentAttachedTerminalId,
  });

  // Call backend reset
  try {
    await invoke("reset_workspace_to_defaults", {
      workspacePath,
      defaultsPath: "defaults",
    });
    logger.info("Backend reset complete", {
      workspacePath,
      source: logPrefix
    });
  } catch (error) {
    logger.error("Failed to reset workspace", error as Error, {
      workspacePath,
      source: logPrefix
    });
    alert(`Failed to reset workspace: ${error}`);
    return;
  }

  // Reset GitHub labels to clean state
  logger.info("Resetting GitHub labels", {
    workspacePath,
    source: logPrefix
  });
  try {
    interface LabelResetResult {
      issues_cleaned: number;
      prs_updated: number;
      errors: string[];
    }

    const labelResult = await invoke<LabelResetResult>("reset_github_labels");
    logger.info("Label reset complete", {
      workspacePath,
      issuesCleaned: labelResult.issues_cleaned,
      prsUpdated: labelResult.prs_updated,
      source: logPrefix
    });

    if (labelResult.errors.length > 0) {
      logger.warn("Label reset errors", {
        workspacePath,
        errors: labelResult.errors,
        source: logPrefix
      });
    }
  } catch (error) {
    logger.warn("Failed to reset GitHub labels", {
      workspacePath,
      error: String(error),
      source: logPrefix
    });
    // Continue anyway - label reset is non-critical
  }

  // Reload config and recreate terminals
  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.setNextTerminalNumber(config.nextAgentNumber);

    // Load agents from fresh config and create terminal sessions for each
    if (config.agents && config.agents.length > 0) {
      // Create new terminal sessions for each agent in the config
      logger.info("Creating terminals", {
        workspacePath,
        terminalCount: config.agents.length,
        source: logPrefix
      });
      for (const agent of config.agents) {
        try {
          // Get instance number
          const instanceNumber = state.getNextTerminalNumber();

          logger.info("Creating terminal", {
            workspacePath,
            terminalName: agent.name,
            instanceNumber,
            role: agent.role || "default",
            source: logPrefix
          });

          // Create terminal in daemon
          const terminalId = await invoke<string>("create_terminal", {
            configId: agent.id,
            name: agent.name,
            workingDir: workspacePath,
            role: agent.role || "default",
            instanceNumber,
          });

          // Update agent ID to match the newly created terminal
          agent.id = terminalId;
          logger.info("Created terminal", {
            workspacePath,
            terminalName: agent.name,
            terminalId,
            source: logPrefix
          });

          // NOTE: Worktrees are now created on-demand when claiming issues, not automatically
          // Agents start in the main workspace directory
          agent.worktreePath = "";
          logger.info("Agent will start in main workspace", {
            workspacePath,
            terminalName: agent.name,
            source: logPrefix
          });
        } catch (error) {
          logger.error("Failed to create terminal", error as Error, {
            workspacePath,
            terminalName: agent.name,
            source: logPrefix
          });
          alert(`Failed to create terminal ${agent.name}: ${error}`);
        }
      }

      logger.info("All terminals created", {
        workspacePath,
        agents: config.agents.map((a) => `${a.name}=${a.id}`),
        source: logPrefix
      });

      // Set workspace as active BEFORE loading agents (needed for proper initialization)
      state.setWorkspace(workspacePath);

      // Now load the agents into state with their new IDs
      logger.info("Loading agents into state", {
        workspacePath,
        agents: config.agents.map((a) => `${a.name}=${a.id}`),
        source: logPrefix
      });
      state.loadAgents(config.agents);
      logger.info("State after loadAgents", {
        workspacePath,
        terminals: state.getTerminals().map((a) => `${a.name}=${a.id}`),
        source: logPrefix
      });

      // IMPORTANT: Save config now with real terminal IDs, BEFORE launching agents
      // This ensures that if we get interrupted (e.g., hot reload), the config has real IDs
      logger.info("Saving config with real terminal IDs", {
        workspacePath,
        source: logPrefix
      });
      const terminalsToSave1 = state.getTerminals();
      const { config: terminalConfigs1, state: terminalStates1 } = splitTerminals(terminalsToSave1);
      await saveConfig({ terminals: terminalConfigs1 });
      await saveState({
        nextAgentNumber: state.getCurrentTerminalNumber(),
        terminals: terminalStates1,
      });
      logger.info("Config saved", {
        workspacePath,
        source: logPrefix
      });

      // Launch agents for terminals with role configs
      logger.info("Launching agents", {
        workspacePath,
        source: logPrefix
      });
      await launchAgentsForTerminals(workspacePath, config.agents);
      logger.info("State after launchAgentsForTerminals", {
        workspacePath,
        terminals: state.getTerminals().map((a) => `${a.name}=${a.id}`),
        source: logPrefix
      });

      // Save final state after agent launch
      logger.info("Saving final config", {
        workspacePath,
        source: logPrefix
      });
      const terminalsToSave2 = state.getTerminals();
      const { config: terminalConfigs2, state: terminalStates2 } = splitTerminals(terminalsToSave2);
      await saveConfig({ terminals: terminalConfigs2 });
      await saveState({
        nextAgentNumber: state.getCurrentTerminalNumber(),
        terminals: terminalStates2,
      });

      logger.info("Workspace reset complete", {
        workspacePath,
        source: logPrefix
      });
    } else {
      // No agents in config - still set workspace as active
      state.setWorkspace(workspacePath);
    }
  } catch (error) {
    logger.error("Failed to reload config after reset", error as Error, {
      workspacePath,
      source: logPrefix
    });
    alert(`Failed to reload config: ${error}`);
  }

  // Re-render
  render();
}
