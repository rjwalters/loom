import { invoke } from "@tauri-apps/api/tauri";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";

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

  // Stop all polling
  const existingTerminals = state.getTerminals();
  existingTerminals.forEach((t) => outputPoller.stopPolling(t.id));

  // Destroy all xterm instances
  terminalManager.destroyAll();

  // Destroy all terminal sessions in daemon (clean up old tmux sessions)
  console.log(`[${logPrefix}] Destroying ${existingTerminals.length} existing terminal sessions`);
  for (const terminal of existingTerminals) {
    try {
      await invoke("destroy_terminal", { id: terminal.id });
      console.log(`[${logPrefix}] Destroyed terminal ${terminal.name} (${terminal.id})`);
    } catch (error) {
      console.warn(`[${logPrefix}] Failed to destroy terminal ${terminal.id}:`, error);
      // Continue anyway - we'll create fresh terminals
    }
  }

  // Kill ALL loom tmux sessions to ensure clean slate
  console.log(`[${logPrefix}] Killing all loom tmux sessions...`);
  try {
    await invoke("kill_all_loom_sessions");
    console.log(`[${logPrefix}] All loom sessions killed`);
  } catch (error) {
    console.warn(`[${logPrefix}] Failed to kill loom sessions:`, error);
    // Continue anyway
  }

  // Clear state (but don't clear config files)
  state.clearAll();
  setCurrentAttachedTerminalId(null);

  // Load existing config (do NOT reset to defaults)
  const { loadWorkspaceConfig, setConfigWorkspace, saveConfig, saveState, splitTerminals } =
    await import("./config");

  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.setNextAgentNumber(config.nextAgentNumber);

    console.log(`[${logPrefix}] Loaded config with ${config.agents?.length || 0} terminals`);

    // Create terminal sessions for each agent in the config
    if (config.agents && config.agents.length > 0) {
      console.log(`[${logPrefix}] Creating ${config.agents.length} terminal sessions...`);

      for (const agent of config.agents) {
        try {
          // Get instance number
          const instanceNumber = state.getNextAgentNumber();

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

          // Create worktree for this terminal
          console.log(`[${logPrefix}] Creating worktree for ${agent.name} (${terminalId})...`);
          const { setupWorktreeForAgent } = await import("./worktree-manager");
          const worktreePath = await setupWorktreeForAgent(terminalId, workspacePath);
          agent.worktreePath = worktreePath;
          console.log(`[${logPrefix}] ✓ Created worktree at ${worktreePath}`);
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
        nextAgentNumber: state.getCurrentAgentNumber(),
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
        nextAgentNumber: state.getCurrentAgentNumber(),
        terminals: terminalStates2,
      });

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
