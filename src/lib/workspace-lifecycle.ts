import { ask } from "@tauri-apps/api/dialog";
import { invoke } from "@tauri-apps/api/tauri";
import {
  loadWorkspaceConfig,
  saveConfig,
  saveState,
  setConfigWorkspace,
  splitTerminals,
} from "./config";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";
import { expandTildePath } from "./workspace-utils";

/**
 * Workspace lifecycle management
 *
 * This module handles workspace path validation, initialization, and loading.
 * It coordinates terminal creation, agent launching, session recovery, and
 * autonomous mode startup.
 */

/**
 * Dependencies for workspace lifecycle operations
 *
 * These dependencies are injected to enable testing and avoid tight coupling
 * to global state and local functions in main.ts
 */
export interface WorkspaceLifecycleDependencies {
  state: AppState;
  validateWorkspacePath: (path: string) => Promise<boolean>;
  launchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
  reconnectTerminals: () => Promise<void>;
  verifyTerminalSessions: () => Promise<void>;
}

/**
 * Handle manual workspace path entry
 *
 * This function:
 * 1. Expands tilde notation (~/) in paths
 * 2. Validates the path is a git repository
 * 3. Initializes .loom/ directory if needed (with user confirmation)
 * 4. Loads existing configuration or creates default terminals
 * 5. Creates tmux sessions for terminals
 * 6. Launches agents with role configurations
 * 7. Reconnects to existing daemon terminals
 * 8. Verifies session health
 * 9. Starts autonomous mode
 * 10. Stores workspace path for next app launch
 *
 * @param path - Workspace path (may contain ~ for home directory)
 * @param deps - Injected dependencies
 */
export async function handleWorkspacePathInput(
  path: string,
  deps: WorkspaceLifecycleDependencies
): Promise<void> {
  const {
    state,
    validateWorkspacePath,
    launchAgentsForTerminals,
    reconnectTerminals,
    verifyTerminalSessions,
  } = deps;

  console.log("[handleWorkspacePathInput] input path:", path);

  // Expand tilde if present
  const expandedPath = await expandTildePath(path);
  console.log("[handleWorkspacePathInput] expanded path:", expandedPath);

  // Always update displayed workspace so bad paths are visible with error message
  state.setDisplayedWorkspace(expandedPath);
  console.log("[handleWorkspacePathInput] set displayedWorkspace, triggering render...");

  const isValid = await validateWorkspacePath(expandedPath);
  console.log("[handleWorkspacePathInput] isValid:", isValid);

  if (!isValid) {
    console.log("[handleWorkspacePathInput] invalid path, stopping");
    return;
  }

  // Check if Loom is initialized in this workspace
  try {
    const isInitialized = await invoke<boolean>("check_loom_initialized", { path: expandedPath });
    console.log("[handleWorkspacePathInput] isInitialized:", isInitialized);

    if (!isInitialized) {
      // Ask user to confirm initialization with detailed information
      const confirmed = await ask(
        `This will create:\n\n` +
          `📁 .loom/ directory with:\n` +
          `  • config.json - Terminal configuration\n` +
          `  • roles/ - Agent role definitions\n\n` +
          `🤖 6 Default Terminals:\n` +
          `  • Shell - Plain shell (primary)\n` +
          `  • Architect - Claude Code worker\n` +
          `  • Curator - Claude Code worker\n` +
          `  • Reviewer - Claude Code worker\n` +
          `  • Worker 1 - Claude Code worker\n` +
          `  • Worker 2 - Claude Code worker\n\n` +
          `📝 .loom/ will be added to .gitignore\n\n` +
          `Continue?`,
        {
          title: "Initialize Loom in this workspace?",
          type: "info",
        }
      );

      if (!confirmed) {
        console.log("[handleWorkspacePathInput] user cancelled initialization");
        return;
      }

      // Initialize workspace using reset_workspace_to_defaults
      try {
        await invoke("reset_workspace_to_defaults", {
          workspacePath: expandedPath,
          defaultsPath: "defaults",
        });
        console.log("[handleWorkspacePathInput] Workspace initialized");
      } catch (error) {
        console.error("Failed to initialize workspace:", error);
        alert(`Failed to initialize workspace: ${error}`);
        return;
      }

      // After initialization, create terminals for the default config
      setConfigWorkspace(expandedPath);
      const config = await loadWorkspaceConfig();
      state.setNextTerminalNumber(config.nextAgentNumber);

      if (config.agents && config.agents.length > 0) {
        console.log("[handleWorkspacePathInput] Creating terminals for fresh workspace");

        // Create terminal sessions for each agent in the config
        for (const agent of config.agents) {
          try {
            // Get instance number
            const instanceNumber = state.getNextTerminalNumber();

            // Create terminal in daemon
            const terminalId = await invoke<string>("create_terminal", {
              configId: agent.id,
              name: agent.name,
              workingDir: expandedPath,
              role: agent.role || "default",
              instanceNumber,
            });

            // Update agent ID to match the newly created terminal
            agent.id = terminalId;
            console.log(
              `[handleWorkspacePathInput] Created terminal ${agent.name} (${terminalId})`
            );
          } catch (error) {
            console.error(
              `[handleWorkspacePathInput] Failed to create terminal ${agent.name}:`,
              error
            );
            alert(`Failed to create terminal ${agent.name}: ${error}`);
          }
        }

        // Load agents into state with their new IDs
        state.loadAgents(config.agents);

        // Launch agents for terminals with role configs
        await launchAgentsForTerminals(expandedPath, config.agents);

        // Save the updated config with real terminal IDs (including worktree paths)
        const terminalsToSave = state.getTerminals();
        const { config: terminalConfigs, state: terminalStates } = splitTerminals(terminalsToSave);
        await saveConfig({ terminals: terminalConfigs });
        await saveState({
          nextAgentNumber: state.getCurrentTerminalNumber(),
          terminals: terminalStates,
        });
        console.log("[handleWorkspacePathInput] Saved config with real terminal IDs");
      }
    } else {
      // Workspace already initialized - load existing config
      setConfigWorkspace(expandedPath);
      const config = await loadWorkspaceConfig();
      state.setNextTerminalNumber(config.nextAgentNumber);

      // Load agents from config
      if (config.agents && config.agents.length > 0) {
        console.log(
          `[handleWorkspacePathInput] Config agents before session creation:`,
          config.agents.map((a) => `${a.name}=${a.id}`)
        );

        // IMPORTANT: Create sessions for migrated terminals with placeholder IDs
        // After migration, terminals have configId but id="__needs_session__"
        let createdSessionCount = 0;
        for (const agent of config.agents) {
          if (agent.id === "__needs_session__") {
            try {
              // Get instance number
              const instanceNumber = state.getNextTerminalNumber();

              console.log(
                `[handleWorkspacePathInput] Creating session for migrated terminal "${agent.name}" (${agent.id})`
              );

              // Create terminal session in daemon
              const sessionId = await invoke<string>("create_terminal", {
                configId: agent.id,
                name: agent.name,
                workingDir: expandedPath,
                role: agent.role || "default",
                instanceNumber,
              });

              // Update agent with real session ID (keep configId stable)
              agent.id = sessionId;
              createdSessionCount++;

              console.log(
                `[handleWorkspacePathInput] ✓ Created session for ${agent.name}: ${sessionId}`
              );
            } catch (error) {
              console.error(
                `[handleWorkspacePathInput] Failed to create session for ${agent.name}:`,
                error
              );
              // Keep placeholder ID - terminal will show as missing session
              // User can use recovery options
            }
          }
        }

        if (createdSessionCount > 0) {
          console.log(
            `[handleWorkspacePathInput] Created ${createdSessionCount} sessions for migrated terminals`
          );
        }

        // Now load agents into state with their session IDs
        console.log(
          `[handleWorkspacePathInput] Config agents before loadAgents:`,
          config.agents.map((a) => `${a.name}=${a.id}`)
        );
        state.loadAgents(config.agents);
        console.log(
          `[handleWorkspacePathInput] State after loadAgents:`,
          state.getTerminals().map((a) => `${a.name}=${a.id}`)
        );

        // If we created sessions, save the updated config with real IDs
        if (createdSessionCount > 0) {
          const terminalsToSave = state.getTerminals();
          const { config: terminalConfigs, state: terminalStates } =
            splitTerminals(terminalsToSave);
          await saveConfig({ terminals: terminalConfigs });
          await saveState({
            nextAgentNumber: state.getCurrentTerminalNumber(),
            terminals: terminalStates,
          });
          console.log(
            `[handleWorkspacePathInput] Saved config with ${createdSessionCount} new session IDs`
          );
        }

        // Reconnect agents to existing daemon terminals
        await reconnectTerminals();

        // Auto-create sessions if ALL terminals are missing (clean slate reset scenario)
        const terminals = state.getTerminals();
        const allMissing = terminals.every((t) => t.missingSession === true);

        if (allMissing && terminals.length > 0) {
          console.log(
            `[handleWorkspacePathInput] All ${terminals.length} terminals missing, auto-creating sessions (clean slate reset recovery)...`
          );

          let createdCount = 0;
          for (const terminal of terminals) {
            try {
              const instanceNumber = state.getNextTerminalNumber();

              console.log(
                `[handleWorkspacePathInput] Auto-creating session for ${terminal.name} (${terminal.id})`
              );

              // Create terminal session in daemon
              const sessionId = await invoke<string>("create_terminal", {
                configId: terminal.id,
                name: terminal.name,
                workingDir: expandedPath,
                role: terminal.role || "default",
                instanceNumber,
              });

              // Update terminal with new session ID and clear error state
              state.updateTerminal(terminal.id, {
                id: sessionId,
                status: TerminalStatus.Idle,
                missingSession: undefined,
              });

              createdCount++;
              console.log(
                `[handleWorkspacePathInput] ✓ Auto-created session for ${terminal.name}: ${sessionId}`
              );
            } catch (error) {
              console.error(
                `[handleWorkspacePathInput] Failed to auto-create session for ${terminal.name}:`,
                error
              );
              // Keep in error state - user can try manual recovery
            }
          }

          if (createdCount > 0) {
            console.log(
              `[handleWorkspacePathInput] Auto-created ${createdCount}/${terminals.length} sessions`
            );

            // Save updated config with new session IDs
            const terminalsToSave = state.getTerminals();
            const { config: terminalConfigs, state: terminalStates } =
              splitTerminals(terminalsToSave);
            await saveConfig({ terminals: terminalConfigs });
            await saveState({
              nextAgentNumber: state.getCurrentTerminalNumber(),
              terminals: terminalStates,
            });

            // Launch agents for terminals with role configs
            console.log(
              `[handleWorkspacePathInput] Launching agents for auto-created terminals...`
            );
            await launchAgentsForTerminals(expandedPath, state.getTerminals());
          }
        }

        // Verify terminal sessions health to clear any stale flags
        await verifyTerminalSessions();
      }
    }

    // Start autonomous mode for eligible terminals
    const { getAutonomousManager } = await import("./autonomous-manager");
    const autonomousManager = getAutonomousManager();
    autonomousManager.startAllAutonomous(state);
    console.log("[handleWorkspacePathInput] Started autonomous agents");

    // Now set workspace as active
    state.setWorkspace(expandedPath);
    console.log("[handleWorkspacePathInput] workspace fully loaded");

    // Store workspace path for next app launch
    try {
      await invoke("set_stored_workspace", { path: expandedPath });
      console.log("[handleWorkspacePathInput] workspace path stored");
    } catch (error) {
      console.error("Failed to store workspace path:", error);
      // Non-fatal - workspace is still loaded
    }
  } catch (error) {
    console.error("Error handling workspace:", error);
    alert(`Error: ${error}`);
  }
}
