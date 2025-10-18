import { invoke } from "@tauri-apps/api/tauri";
import { Logger } from "./logger";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { cleanupWorkspace } from "./workspace-cleanup";

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

  // Load existing config (do NOT reset to defaults)
  const { loadWorkspaceConfig, setConfigWorkspace, saveConfig, saveState, splitTerminals } =
    await import("./config");

  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.setNextTerminalNumber(config.nextAgentNumber);

    logger.info("Loaded config", {
      workspacePath,
      terminalCount: config.agents?.length || 0,
      source: logPrefix
    });

    // Create terminal sessions for each agent in the config
    if (config.agents && config.agents.length > 0) {
      logger.info("Creating terminal sessions", {
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

      // Set workspace as active BEFORE loading agents
      state.setWorkspace(workspacePath);

      // Load agents into state with their new session IDs
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

      // Save config with real terminal IDs BEFORE launching agents
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

      // Brief delay to allow tmux sessions to stabilize after agent launch
      // Without this delay, health checks may run before tmux sessions are fully query-able
      logger.info("Waiting for tmux sessions to stabilize (500ms)", {
        workspacePath,
        source: logPrefix
      });
      await new Promise((resolve) => setTimeout(resolve, 500));

      // Trigger immediate health check to verify terminal sessions exist
      // This prevents false "missing session" errors on startup
      logger.info("Running immediate health check", {
        workspacePath,
        source: logPrefix
      });
      const { getHealthMonitor } = await import("./health-monitor");
      const healthMonitor = getHealthMonitor();
      await healthMonitor.performHealthCheck();
      logger.info("Health check complete", {
        workspacePath,
        source: logPrefix
      });

      // Mark all terminals as health-checked to prevent redundant checks in render loop
      const terminalIds = state.getTerminals().map((t) => t.id);
      dependencies.markTerminalsHealthChecked(terminalIds);
      logger.info("Marked terminals as health-checked", {
        workspacePath,
        terminalCount: terminalIds.length,
        terminalIds,
        source: logPrefix
      });

      logger.info("Workspace engine started successfully", {
        workspacePath,
        source: logPrefix
      });
    } else {
      // No agents in config - still set workspace as active
      state.setWorkspace(workspacePath);
      logger.info("No terminals configured, workspace active with empty state", {
        workspacePath,
        source: logPrefix
      });
    }
  } catch (error) {
    logger.error("Failed to start engine", error as Error, {
      workspacePath,
      source: logPrefix
    });
    alert(`Failed to start Loom engine: ${error}`);
  }

  // Re-render
  render();
}
