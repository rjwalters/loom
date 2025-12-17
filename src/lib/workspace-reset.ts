import { invoke } from "@tauri-apps/api/core";
import { loadWorkspaceConfig, saveCurrentConfiguration, setConfigWorkspace } from "./config";
import type {
  AgentLauncherDependencies,
  CoreDependencies,
  RenderableDependencies,
  TerminalAttachmentDependencies,
  TerminalInfrastructureDependencies,
} from "./dependencies";
import { Logger } from "./logger";
import { createTerminalsWithRetry, type TerminalConfig } from "./parallel-terminal-creator";
import { showToast } from "./toast";
import { cleanupWorkspace } from "./workspace-cleanup";

const logger = Logger.forComponent("workspace-reset");

/**
 * Dependencies needed by the workspace reset logic
 */
export interface WorkspaceResetDependencies
  extends CoreDependencies,
    TerminalInfrastructureDependencies,
    TerminalAttachmentDependencies,
    AgentLauncherDependencies,
    RenderableDependencies {}

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
    source: logPrefix,
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
      source: logPrefix,
    });
  } catch (error) {
    logger.error("Failed to reset workspace", error as Error, {
      workspacePath,
      source: logPrefix,
    });
    showToast(`Failed to reset workspace: ${error}`, "error");
    return;
  }

  // Reset GitHub labels to clean state
  logger.info("Resetting GitHub labels", {
    workspacePath,
    source: logPrefix,
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
      source: logPrefix,
    });

    if (labelResult.errors.length > 0) {
      logger.warn("Label reset errors", {
        workspacePath,
        errors: labelResult.errors,
        source: logPrefix,
      });
    }
  } catch (error) {
    logger.warn("Failed to reset GitHub labels", {
      workspacePath,
      error: String(error),
      source: logPrefix,
    });
    // Continue anyway - label reset is non-critical
  }

  // Reload config and recreate terminals
  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.terminals.setNextTerminalNumber(config.nextAgentNumber);

    // Load agents from fresh config and create terminal sessions for each
    if (config.agents && config.agents.length > 0) {
      logger.info("Creating terminals in parallel", {
        workspacePath,
        terminalCount: config.agents.length,
        source: logPrefix,
      });

      // Build array of terminal configurations for parallel creation
      const terminalConfigs: TerminalConfig[] = config.agents.map((agent) => ({
        id: agent.id,
        name: agent.name,
        role: agent.role || "default",
        workingDir: workspacePath,
        instanceNumber: 0, // Will be assigned by createTerminalsWithRetry
      }));

      // Create all terminals in parallel with automatic retry
      const { succeeded, failed } = await createTerminalsWithRetry(
        terminalConfigs,
        workspacePath,
        state
      );

      // Update agent IDs for succeeded terminals
      for (const success of succeeded) {
        const agent = config.agents.find((a) => a.id === success.configId);
        if (agent) {
          agent.id = success.terminalId;
          // NOTE: Worktrees are now created on-demand when claiming issues, not automatically
          // Agents start in the main workspace directory
          agent.worktreePath = "";
          logger.info("Agent will start in main workspace", {
            workspacePath,
            terminalName: agent.name,
            terminalId: success.terminalId,
            source: logPrefix,
          });
        }
      }

      // Report failures to user
      if (failed.length > 0) {
        const failedNames = failed
          .map((f) => {
            const agent = config.agents.find((a) => a.id === f.configId);
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
          `Failed to create ${failed.length} terminal(s) after retries: ${failedNames}. Successfully created ${succeeded.length} of ${config.agents.length} terminals.`,
          "error",
          7000
        );
      }

      logger.info("Parallel terminal creation complete", {
        workspacePath,
        totalTerminals: config.agents.length,
        succeeded: succeeded.length,
        failed: failed.length,
        agents: config.agents.map((a) => `${a.name}=${a.id}`),
        source: logPrefix,
      });

      // Set workspace as active BEFORE loading agents (needed for proper initialization)
      state.workspace.setWorkspace(workspacePath);

      // Now load the agents into state with their new IDs
      logger.info("Loading agents into state", {
        workspacePath,
        agents: config.agents.map((a) => `${a.name}=${a.id}`),
        source: logPrefix,
      });
      state.terminals.loadTerminals(config.agents);
      logger.info("State after loadAgents", {
        workspacePath,
        terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
        source: logPrefix,
      });

      // IMPORTANT: Save config now with real terminal IDs, BEFORE launching agents
      // This ensures that if we get interrupted (e.g., hot reload), the config has real IDs
      logger.info("Saving config with real terminal IDs", {
        workspacePath,
        source: logPrefix,
      });
      await saveCurrentConfiguration(state);
      logger.info("Config saved", {
        workspacePath,
        source: logPrefix,
      });

      // Launch agents for terminals with role configs
      logger.info("Launching agents", {
        workspacePath,
        source: logPrefix,
      });
      await launchAgentsForTerminals(workspacePath, config.agents);
      logger.info("State after launchAgentsForTerminals", {
        workspacePath,
        terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
        source: logPrefix,
      });

      // Save final state after agent launch
      logger.info("Saving final config", {
        workspacePath,
        source: logPrefix,
      });
      await saveCurrentConfiguration(state);

      logger.info("Workspace reset complete", {
        workspacePath,
        source: logPrefix,
      });
    } else {
      // No agents in config - still set workspace as active
      state.workspace.setWorkspace(workspacePath);
    }
  } catch (error) {
    logger.error("Failed to reload config after reset", error as Error, {
      workspacePath,
      source: logPrefix,
    });
    showToast(`Failed to reload config: ${error}`, "error");
  }

  // Re-render
  render();
}
