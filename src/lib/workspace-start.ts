import type {
  AgentLauncherDependencies,
  CoreDependencies,
  RenderableDependencies,
  TerminalAttachmentDependencies,
  TerminalInfrastructureDependencies,
} from "./dependencies";
import { Logger } from "./logger";
import { createTerminalsWithRetry, type TerminalConfig } from "./parallel-terminal-creator";
import { TERMINAL_OUTPUT_STABILIZATION_MS } from "./timing-constants";
import { showToast } from "./toast";
import { cleanupWorkspace } from "./workspace-cleanup";

const logger = Logger.forComponent("workspace-start");

/**
 * Dependencies needed by the workspace start logic
 */
export interface WorkspaceStartDependencies
  extends CoreDependencies,
    TerminalInfrastructureDependencies,
    TerminalAttachmentDependencies,
    AgentLauncherDependencies,
    RenderableDependencies {
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
  });

  // Load existing config (do NOT reset to defaults)
  const { loadWorkspaceConfig, setConfigWorkspace, saveCurrentConfiguration } = await import(
    "./config"
  );

  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.terminals.setNextTerminalNumber(config.nextAgentNumber);
    state.setOfflineMode(config.offlineMode);

    logger.info("Loaded config", {
      workspacePath,
      terminalCount: config.agents?.length || 0,
      offlineMode: config.offlineMode,
      source: logPrefix,
    });

    // Create terminal sessions for each agent in the config
    if (config.agents && config.agents.length > 0) {
      logger.info("Creating terminal sessions in parallel", {
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

      // Set workspace as active BEFORE loading agents
      state.workspace.setWorkspace(workspacePath);

      // Load agents into state with their new session IDs
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
        terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
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
      logger.info("Waiting for tmux sessions to stabilize", {
        workspacePath,
        delayMs: TERMINAL_OUTPUT_STABILIZATION_MS,
        source: logPrefix,
      });
      await new Promise((resolve) => setTimeout(resolve, TERMINAL_OUTPUT_STABILIZATION_MS));

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
      const terminalIds = state.terminals.getTerminals().map((t) => t.id);
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
      state.workspace.setWorkspace(workspacePath);
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
