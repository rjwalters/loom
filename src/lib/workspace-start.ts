import { invoke } from "@tauri-apps/api/tauri";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";
import { cleanupWorkspace } from "./workspace-cleanup";

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

  console.log(`[${logPrefix}] Starting Loom engine for workspace`);

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

    console.log(`[${logPrefix}] Loaded config with ${config.agents?.length || 0} terminals`);

    // Create terminal sessions for each agent in the config
    if (config.agents && config.agents.length > 0) {
      console.log(`[${logPrefix}] Creating ${config.agents.length} terminal sessions...`);

      for (const agent of config.agents) {
        try {
          // Get instance number
          const instanceNumber = state.getNextTerminalNumber();

          console.log(
            `[${logPrefix}] Creating terminal "${agent.name}" with instance ${instanceNumber}, role=${agent.role || "default"}, workingDir=${workspacePath}`
          );

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
          console.log(`[${logPrefix}] ✓ Created terminal ${agent.name} (${terminalId})`);

          // NOTE: Worktrees are now created on-demand when claiming issues, not automatically
          // Agents start in the main workspace directory
          agent.worktreePath = "";
          console.log(`[${logPrefix}] ✓ Agent will start in main workspace`);
        } catch (error) {
          console.error(`[${logPrefix}] ✗ Failed to create terminal ${agent.name}:`, error);
          alert(`Failed to create terminal ${agent.name}: ${error}`);
        }
      }

      console.log(
        `[${logPrefix}] All terminals created, agents array:`,
        config.agents.map((a) => `${a.name}=${a.id}`)
      );

      // Set workspace as active BEFORE loading agents
      state.setWorkspace(workspacePath);

      // Load agents into state with their new session IDs
      console.log(
        `[${logPrefix}] Loading agents into state:`,
        config.agents.map((a) => `${a.name}=${a.id}`)
      );
      state.loadAgents(config.agents);
      console.log(
        `[${logPrefix}] State after loadAgents:`,
        state.getTerminals().map((a) => `${a.name}=${a.id}`)
      );

      // Save config with real terminal IDs BEFORE launching agents
      console.log(`[${logPrefix}] Saving config with real terminal IDs...`);
      const terminalsToSave1 = state.getTerminals();
      const { config: terminalConfigs1, state: terminalStates1 } = splitTerminals(terminalsToSave1);
      await saveConfig({ terminals: terminalConfigs1 });
      await saveState({
        nextAgentNumber: state.getCurrentTerminalNumber(),
        terminals: terminalStates1,
      });
      console.log(`[${logPrefix}] Config saved`);

      // Launch agents for terminals with role configs
      console.log(`[${logPrefix}] Launching agents...`);
      await launchAgentsForTerminals(workspacePath, config.agents);
      console.log(
        `[${logPrefix}] State after launchAgentsForTerminals:`,
        state.getTerminals().map((a) => `${a.name}=${a.id}`)
      );

      // Save final state after agent launch
      console.log(`[${logPrefix}] Saving final config...`);
      const terminalsToSave2 = state.getTerminals();
      const { config: terminalConfigs2, state: terminalStates2 } = splitTerminals(terminalsToSave2);
      await saveConfig({ terminals: terminalConfigs2 });
      await saveState({
        nextAgentNumber: state.getCurrentTerminalNumber(),
        terminals: terminalStates2,
      });

      // Brief delay to allow tmux sessions to stabilize after agent launch
      // Without this delay, health checks may run before tmux sessions are fully query-able
      console.log(`[${logPrefix}] Waiting for tmux sessions to stabilize (500ms)...`);
      await new Promise((resolve) => setTimeout(resolve, 500));

      // Trigger immediate health check to verify terminal sessions exist
      // This prevents false "missing session" errors on startup
      console.log(`[${logPrefix}] Running immediate health check...`);
      const { getHealthMonitor } = await import("./health-monitor");
      const healthMonitor = getHealthMonitor();
      await healthMonitor.performHealthCheck();
      console.log(`[${logPrefix}] Health check complete`);

      // Mark all terminals as health-checked to prevent redundant checks in render loop
      const terminalIds = state.getTerminals().map((t) => t.id);
      dependencies.markTerminalsHealthChecked(terminalIds);
      console.log(
        `[${logPrefix}] Marked ${terminalIds.length} terminals as health-checked:`,
        terminalIds
      );

      console.log(`[${logPrefix}] Workspace engine started successfully`);
    } else {
      // No agents in config - still set workspace as active
      state.setWorkspace(workspacePath);
      console.log(`[${logPrefix}] No terminals configured, workspace active with empty state`);
    }
  } catch (error) {
    console.error(`[${logPrefix}] Failed to start engine:`, error);
    alert(`Failed to start Loom engine: ${error}`);
  }

  // Re-render
  render();
}
