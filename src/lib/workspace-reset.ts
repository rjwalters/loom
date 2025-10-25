import { invoke } from "@tauri-apps/api/core";
import { loadWorkspaceConfig, saveCurrentConfiguration, setConfigWorkspace } from "./config";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import { createTerminalsWithRetry, type TerminalConfig } from "./parallel-terminal-creator";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { showToast } from "./toast";
import { cleanupWorkspace } from "./workspace-cleanup";
import {
  createTerminalWorktreesInParallel,
  type TerminalWorktreeConfig,
} from "./terminal-worktree-manager";

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
    source: logPrefix,
  });

  // Cleanup existing terminals and sessions
  await cleanupWorkspace({
    component: logPrefix,
    state,
    outputPoller,
    terminalManager,
    setCurrentAttachedTerminalId,
    workspacePath,
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
    state.setNextTerminalNumber(config.nextAgentNumber);

    // Load agents from fresh config and create terminal sessions for each
    if (config.agents && config.agents.length > 0) {
      logger.info("Creating terminal worktrees and sessions in parallel", {
        workspacePath,
        terminalCount: config.agents.length,
        source: logPrefix,
      });

      // Step 1: Create terminal worktrees with role-specific CLAUDE.md files
      const worktreeConfigs: TerminalWorktreeConfig[] = config.agents.map((agent) => ({
        terminalId: agent.id,
        terminalName: agent.name,
        roleFile: (agent.roleConfig?.roleFile as string | undefined) || "driver.md", // Default to driver if no role file
        workspacePath,
      }));

      const { succeeded: worktreeSuccess, failed: worktreeFailed } =
        await createTerminalWorktreesInParallel(worktreeConfigs);

      // Log worktree creation results
      logger.info("Terminal worktree creation complete", {
        workspacePath,
        succeeded: worktreeSuccess.length,
        failed: worktreeFailed.length,
        source: logPrefix,
      });

      // Report worktree failures to user
      if (worktreeFailed.length > 0) {
        const failedNames = worktreeFailed
          .map((f) => {
            const agent = config.agents.find((a) => a.id === f.terminalId);
            return agent?.name || f.terminalId;
          })
          .join(", ");

        logger.error(
          "Some terminal worktrees failed to create",
          new Error("Worktree creation failures"),
          {
            workspacePath,
            failedCount: worktreeFailed.length,
            failedNames,
            failures: worktreeFailed,
            source: logPrefix,
          }
        );

        showToast(
          `Failed to create worktrees for ${worktreeFailed.length} terminal(s): ${failedNames}. These terminals will start in the main workspace.`,
          "info",
          7000
        );
      }

      // Step 2: Build array of terminal configurations for parallel creation
      // Use worktree paths for terminals with successful worktree creation
      const worktreePathMap = new Map(
        worktreeSuccess.map((w) => [w.terminalId, w.worktreePath])
      );

      const terminalConfigs: TerminalConfig[] = config.agents.map((agent) => ({
        id: agent.id,
        name: agent.name,
        role: agent.role || "default",
        workingDir: worktreePathMap.get(agent.id) || workspacePath, // Use worktree path if available
        instanceNumber: 0, // Will be assigned by createTerminalsWithRetry
      }));

      // Create all terminals in parallel with automatic retry
      const { succeeded, failed } = await createTerminalsWithRetry(
        terminalConfigs,
        workspacePath,
        state
      );

      // Update agent IDs and worktree paths for succeeded terminals
      for (const success of succeeded) {
        const agent = config.agents.find((a) => a.id === success.configId);
        if (agent) {
          agent.id = success.terminalId;
          // Set worktree path if worktree was created successfully
          const worktreePath = worktreePathMap.get(success.configId);
          agent.worktreePath = worktreePath || "";

          if (worktreePath) {
            logger.info("Agent will start in terminal worktree", {
              workspacePath,
              terminalName: agent.name,
              terminalId: success.terminalId,
              worktreePath,
              source: logPrefix,
            });
          } else {
            logger.info("Agent will start in main workspace (no worktree)", {
              workspacePath,
              terminalName: agent.name,
              terminalId: success.terminalId,
              source: logPrefix,
            });
          }
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
      state.setWorkspace(workspacePath);

      // Now load the agents into state with their new IDs
      logger.info("Loading agents into state", {
        workspacePath,
        agents: config.agents.map((a) => `${a.name}=${a.id}`),
        source: logPrefix,
      });
      state.loadAgents(config.agents);
      logger.info("State after loadAgents", {
        workspacePath,
        terminals: state.getTerminals().map((a) => `${a.name}=${a.id}`),
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
        terminals: state.getTerminals().map((a) => `${a.name}=${a.id}`),
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
      state.setWorkspace(workspacePath);
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
