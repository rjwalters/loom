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

const logger = Logger.forComponent("workspace-start");

/**
 * Dependencies needed by the workspace start logic
 */
export interface WorkspaceStartDependencies {
  state: AppState;
  outputPoller: OutputPoller;
  terminalManager: TerminalManager;
  setCurrentAttachedTerminalId: (id: string | null) => void;
  launchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
  render: () => void;
  markTerminalsHealthChecked: (terminalIds: string[]) => void;
}

/**
 * Start the Loom engine for the current workspace
 *
 * This function:
 * 1. Reads existing .loom/config.json (does NOT overwrite with defaults)
 * 2. Creates terminal sessions for each configured terminal
 * 3. Launches agents for terminals with role configs
 * 4. Starts autonomous mode
 *
 * This is the normal way to "start Loom" - it uses your existing configuration.
 * Use factory reset if you want to overwrite with default config.
 *
 * @param workspacePath - The workspace directory path
 * @param dependencies - Dependencies (state, managers, callbacks)
 * @param logPrefix - Prefix for console.log messages
 */
export async function startWorkspaceEngine(
  workspacePath: string,
  dependencies: WorkspaceStartDependencies,
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

  logger.info("Starting Loom engine for workspace", {
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

  // Load existing config (do NOT reset to defaults)
  const { loadWorkspaceConfig, setConfigWorkspace, saveCurrentConfiguration } = await import(
    "./config"
  );

  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.setNextTerminalNumber(config.nextAgentNumber);
    state.setOfflineMode(config.offlineMode);

    logger.info("Loaded config", {
      workspacePath,
      terminalCount: config.agents?.length || 0,
      offlineMode: config.offlineMode,
      source: logPrefix,
    });

    // Create terminal sessions for each agent in the config
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

      // Set workspace as active BEFORE loading agents
      state.setWorkspace(workspacePath);

      // Load agents into state with their new session IDs
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

      // Save config with real terminal IDs BEFORE launching agents
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

      // Brief delay to allow tmux sessions to stabilize after agent launch
      // Without this delay, health checks may run before tmux sessions are fully query-able
      logger.info("Waiting for tmux sessions to stabilize (500ms)", {
        workspacePath,
        source: logPrefix,
      });
      await new Promise((resolve) => setTimeout(resolve, 500));

      // Trigger immediate health check to verify terminal sessions exist
      // This prevents false "missing session" errors on startup
      logger.info("Running immediate health check", {
        workspacePath,
        source: logPrefix,
      });
      const { getHealthMonitor } = await import("./health-monitor");
      const healthMonitor = getHealthMonitor();
      await healthMonitor.performHealthCheck();
      logger.info("Health check complete", {
        workspacePath,
        source: logPrefix,
      });

      // Mark all terminals as health-checked to prevent redundant checks in render loop
      const terminalIds = state.getTerminals().map((t) => t.id);
      dependencies.markTerminalsHealthChecked(terminalIds);
      logger.info("Marked terminals as health-checked", {
        workspacePath,
        terminalCount: terminalIds.length,
        terminalIds,
        source: logPrefix,
      });

      logger.info("Workspace engine started successfully", {
        workspacePath,
        source: logPrefix,
      });
    } else {
      // No agents in config - still set workspace as active
      state.setWorkspace(workspacePath);
      logger.info("No terminals configured, workspace active with empty state", {
        workspacePath,
        source: logPrefix,
      });
    }
  } catch (error) {
    logger.error("Failed to start engine", error as Error, {
      workspacePath,
      source: logPrefix,
    });
    showToast(`Failed to start Loom engine: ${error}`, "error");
  }

  // Re-render
  render();
}
