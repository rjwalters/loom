import { invoke } from "@tauri-apps/api/tauri";
import {
  loadWorkspaceConfig,
  saveConfig,
  saveState,
  setConfigWorkspace,
  splitTerminals,
} from "./config";
import type { OutputPoller } from "./output-poller";
import type { AppState, Terminal } from "./state";
import type { TerminalManager } from "./terminal-manager";

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

  console.log(`[${logPrefix}] Resetting workspace to defaults`);

  // Stop all polling
  const terminals = state.getTerminals();
  terminals.forEach((t) => outputPoller.stopPolling(t.id));

  // Destroy all xterm instances
  terminalManager.destroyAll();

  // Destroy all terminal sessions in daemon (clean up tmux sessions)
  console.log(`[${logPrefix}] Destroying ${terminals.length} terminal sessions`);
  for (const terminal of terminals) {
    try {
      await invoke("destroy_terminal", { id: terminal.id });
      console.log(`[${logPrefix}] Destroyed terminal ${terminal.name} (${terminal.id})`);
    } catch (error) {
      console.warn(`[${logPrefix}] Failed to destroy terminal ${terminal.id}:`, error);
      // Continue anyway - we'll create fresh terminals
    }
  }

  // Kill ALL loom tmux sessions to ensure clean slate
  // This is necessary because daemon may restore old sessions on startup
  console.log(`[${logPrefix}] Killing all loom tmux sessions...`);
  try {
    await invoke("kill_all_loom_sessions");
    console.log(`[${logPrefix}] All loom sessions killed`);
  } catch (error) {
    console.warn(`[${logPrefix}] Failed to kill loom sessions:`, error);
    // Continue anyway - we'll try to create fresh terminals
  }

  // Call backend reset
  try {
    await invoke("reset_workspace_to_defaults", {
      workspacePath,
      defaultsPath: "defaults",
    });
    console.log(`[${logPrefix}] Backend reset complete`);
  } catch (error) {
    console.error("Failed to reset workspace:", error);
    alert(`Failed to reset workspace: ${error}`);
    return;
  }

  // Reset GitHub labels to clean state
  console.log(`[${logPrefix}] Resetting GitHub labels...`);
  try {
    interface LabelResetResult {
      issues_cleaned: number;
      prs_updated: number;
      errors: string[];
    }

    const labelResult = await invoke<LabelResetResult>("reset_github_labels");
    console.log(
      `[${logPrefix}] Label reset complete: ${labelResult.issues_cleaned} issues cleaned, ${labelResult.prs_updated} PRs updated`
    );

    if (labelResult.errors.length > 0) {
      console.warn(`[${logPrefix}] Label reset errors:`, labelResult.errors);
    }
  } catch (error) {
    console.warn(`[${logPrefix}] Failed to reset GitHub labels:`, error);
    // Continue anyway - label reset is non-critical
  }

  // Clear state
  state.clearAll();
  setCurrentAttachedTerminalId(null);

  // Reload config and recreate terminals
  try {
    setConfigWorkspace(workspacePath);
    const config = await loadWorkspaceConfig();
    state.setNextAgentNumber(config.nextAgentNumber);

    // Load agents from fresh config and create terminal sessions for each
    if (config.agents && config.agents.length > 0) {
      // Create new terminal sessions for each agent in the config
      console.log(`[${logPrefix}] Creating ${config.agents.length} terminals...`);
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
        } catch (error) {
          console.error(`[${logPrefix}] ✗ Failed to create terminal ${agent.name}:`, error);
          alert(`Failed to create terminal ${agent.name}: ${error}`);
        }
      }

      console.log(
        `[${logPrefix}] All terminals created, agents array:`,
        config.agents.map((a) => `${a.name}=${a.id}`)
      );

      // Set workspace as active BEFORE loading agents (needed for proper initialization)
      state.setWorkspace(workspacePath);

      // Now load the agents into state with their new IDs
      console.log(
        `[${logPrefix}] Loading agents into state:`,
        config.agents.map((a) => `${a.name}=${a.id}`)
      );
      state.loadAgents(config.agents);
      console.log(
        `[${logPrefix}] State after loadAgents:`,
        state.getTerminals().map((a) => `${a.name}=${a.id}`)
      );

      // IMPORTANT: Save config now with real terminal IDs, BEFORE launching agents
      // This ensures that if we get interrupted (e.g., hot reload), the config has real IDs
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

      // Save again with worktree paths added by agent launch
      console.log(`[${logPrefix}] Saving config with worktree paths...`);
      const terminalsToSave2 = state.getTerminals();
      const { config: terminalConfigs2, state: terminalStates2 } = splitTerminals(terminalsToSave2);
      await saveConfig({ terminals: terminalConfigs2 });
      await saveState({
        nextAgentNumber: state.getCurrentAgentNumber(),
        terminals: terminalStates2,
      });

      console.log(`[${logPrefix}] Workspace reset complete`);
    } else {
      // No agents in config - still set workspace as active
      state.setWorkspace(workspacePath);
    }
  } catch (error) {
    console.error("Failed to reload config after reset:", error);
    alert(`Failed to reload config: ${error}`);
  }

  // Re-render
  render();
}
