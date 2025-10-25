import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { loadWorkspaceConfig, saveCurrentConfiguration, setConfigWorkspace } from "./config";
import { Logger } from "./logger";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";
import { showToast } from "./toast";
import { expandTildePath } from "./workspace-utils";

const logger = Logger.forComponent("workspace-lifecycle");

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
 * Expand and validate workspace path
 *
 * @param path - Raw path input (may contain ~)
 * @param deps - Dependencies
 * @returns Expanded path if valid, null otherwise
 */
async function expandAndValidatePath(
  path: string,
  deps: WorkspaceLifecycleDependencies
): Promise<string | null> {
  logger.info("Handling workspace path input", { path });

  // Expand tilde if present
  const expandedPath = await expandTildePath(path);
  logger.info("Expanded path", { expandedPath });

  // Always update displayed workspace so bad paths are visible with error message
  deps.state.setDisplayedWorkspace(expandedPath);
  logger.info("Set displayedWorkspace, triggering render");

  const isValid = await deps.validateWorkspacePath(expandedPath);
  logger.info("Workspace path validation complete", { isValid });

  if (!isValid) {
    logger.info("Invalid path, stopping");
    return null;
  }

  return expandedPath;
}

/**
 * Ensure repository scaffolding is set up
 *
 * Creates CLAUDE.md, .claude/, .codex/, AGENTS.md if needed
 *
 * @param workspacePath - Path to workspace
 */
async function ensureWorkspaceScaffolding(workspacePath: string): Promise<void> {
  try {
    await invoke("ensure_workspace_scaffolding", { workspacePath });
    logger.info("Repository scaffolding ensured", { workspacePath });
  } catch (error) {
    logger.warn("Failed to ensure repository scaffolding", {
      workspacePath,
      error: String(error),
    });
    // Non-fatal - workspace can still be used, but some features may not work optimally
  }
}

/**
 * Initialize a new workspace with default configuration
 *
 * Creates .loom/ directory, default terminals, and launches agents
 *
 * @param workspacePath - Path to workspace
 * @param deps - Dependencies
 */
async function initializeNewWorkspace(
  workspacePath: string,
  deps: WorkspaceLifecycleDependencies
): Promise<void> {
  const { state, launchAgentsForTerminals } = deps;

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
      kind: "info",
    }
  );

  if (!confirmed) {
    logger.info("User cancelled initialization");
    return;
  }

  // Initialize workspace using reset_workspace_to_defaults
  try {
    await invoke("reset_workspace_to_defaults", {
      workspacePath,
      defaultsPath: "defaults",
    });
    logger.info("Workspace initialized", { workspacePath });
  } catch (error) {
    logger.error("Failed to initialize workspace", error as Error, {
      workspacePath,
    });
    showToast(`Failed to initialize workspace: ${error}`, "error");
    return;
  }

  // After initialization, create terminals for the default config
  setConfigWorkspace(workspacePath);
  const config = await loadWorkspaceConfig();
  state.setNextTerminalNumber(config.nextAgentNumber);

  if (config.agents && config.agents.length > 0) {
    logger.info("Creating terminals for fresh workspace", {
      workspacePath,
      terminalCount: config.agents.length,
    });

    // Create terminal sessions for each agent in the config
    for (const agent of config.agents) {
      try {
        // Get instance number
        const instanceNumber = state.getNextTerminalNumber();

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
          terminalName: agent.name,
          terminalId,
          workspacePath,
        });
      } catch (error) {
        logger.error("Failed to create terminal", error as Error, {
          terminalName: agent.name,
          workspacePath,
        });
        showToast(`Failed to create terminal ${agent.name}: ${error}`, "error");
      }
    }

    // Load agents into state with their new IDs
    state.loadAgents(config.agents);

    // Launch agents for terminals with role configs
    await launchAgentsForTerminals(workspacePath, config.agents);

    // Save the updated config with real terminal IDs (including worktree paths)
    await saveCurrentConfiguration(state);
    logger.info("Saved config with real terminal IDs", { workspacePath });
  }
}

/**
 * Create sessions for migrated terminals with placeholder IDs
 *
 * After migration, terminals have configId but id="__needs_session__"
 *
 * @param config - Workspace configuration
 * @param workspacePath - Path to workspace
 * @param state - App state
 * @returns Number of sessions created
 */
async function createSessionsForMigratedTerminals(
  config: { agents: Terminal[]; nextAgentNumber: number },
  workspacePath: string,
  state: AppState
): Promise<number> {
  let createdSessionCount = 0;

  for (const agent of config.agents) {
    if (agent.id === "__needs_session__") {
      try {
        // Get instance number
        const instanceNumber = state.getNextTerminalNumber();

        logger.info("Creating session for migrated terminal", {
          workspacePath,
          terminalName: agent.name,
          currentId: agent.id,
        });

        // Create terminal session in daemon
        const sessionId = await invoke<string>("create_terminal", {
          configId: agent.id,
          name: agent.name,
          workingDir: workspacePath,
          role: agent.role || "default",
          instanceNumber,
        });

        // Update agent with real session ID (keep configId stable)
        agent.id = sessionId;
        createdSessionCount++;

        logger.info("Created session for migrated terminal", {
          workspacePath,
          terminalName: agent.name,
          sessionId,
        });
      } catch (error) {
        logger.error("Failed to create session for migrated terminal", error as Error, {
          workspacePath,
          terminalName: agent.name,
        });
        // Keep placeholder ID - terminal will show as missing session
        // User can use recovery options
      }
    }
  }

  if (createdSessionCount > 0) {
    logger.info("Created sessions for migrated terminals", {
      workspacePath,
      createdCount: createdSessionCount,
    });
  }

  return createdSessionCount;
}

/**
 * Auto-create sessions when ALL terminals are missing
 *
 * This handles the clean slate reset scenario where all sessions need recreation
 *
 * @param workspacePath - Path to workspace
 * @param deps - Dependencies
 * @returns Number of sessions created
 */
async function autoCreateSessionsIfAllMissing(
  workspacePath: string,
  deps: WorkspaceLifecycleDependencies
): Promise<number> {
  const { state, launchAgentsForTerminals } = deps;
  const terminals = state.getTerminals();
  const allMissing = terminals.every((t) => t.missingSession === true);

  if (!allMissing || terminals.length === 0) {
    return 0;
  }

  logger.info("All terminals missing, auto-creating sessions (clean slate reset recovery)", {
    workspacePath,
    terminalCount: terminals.length,
  });

  let createdCount = 0;
  for (const terminal of terminals) {
    try {
      const instanceNumber = state.getNextTerminalNumber();

      logger.info("Auto-creating session for terminal", {
        workspacePath,
        terminalName: terminal.name,
        terminalId: terminal.id,
      });

      // Create terminal session in daemon
      const sessionId = await invoke<string>("create_terminal", {
        configId: terminal.id,
        name: terminal.name,
        workingDir: workspacePath,
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
      logger.info("Auto-created session for terminal", {
        workspacePath,
        terminalName: terminal.name,
        sessionId,
      });
    } catch (error) {
      logger.error("Failed to auto-create session for terminal", error as Error, {
        workspacePath,
        terminalName: terminal.name,
      });
      // Keep in error state - user can try manual recovery
    }
  }

  if (createdCount > 0) {
    logger.info("Auto-created sessions", {
      workspacePath,
      createdCount,
      totalCount: terminals.length,
    });

    // Save updated config with new session IDs
    await saveCurrentConfiguration(state);

    // Launch agents for terminals with role configs
    logger.info("Launching agents for auto-created terminals", {
      workspacePath,
    });
    await launchAgentsForTerminals(workspacePath, state.getTerminals());
  }

  return createdCount;
}

/**
 * Load an existing workspace with configuration
 *
 * Loads config, creates missing sessions, reconnects terminals, and verifies health
 *
 * @param workspacePath - Path to workspace
 * @param deps - Dependencies
 */
async function loadExistingWorkspace(
  workspacePath: string,
  deps: WorkspaceLifecycleDependencies
): Promise<void> {
  const { state, reconnectTerminals, verifyTerminalSessions } = deps;

  // Workspace already initialized - load existing config
  setConfigWorkspace(workspacePath);
  const config = await loadWorkspaceConfig();
  state.setNextTerminalNumber(config.nextAgentNumber);

  // Load agents from config
  if (config.agents && config.agents.length > 0) {
    logger.info("Config agents before session creation", {
      workspacePath,
      agents: config.agents.map((a) => `${a.name}=${a.id}`),
    });

    // Create sessions for migrated terminals with placeholder IDs
    const createdSessionCount = await createSessionsForMigratedTerminals(
      config,
      workspacePath,
      state
    );

    // Now load agents into state with their session IDs
    logger.info("Config agents before loadAgents", {
      workspacePath,
      agents: config.agents.map((a) => `${a.name}=${a.id}`),
    });
    state.loadAgents(config.agents);
    logger.info("State after loadAgents", {
      workspacePath,
      terminals: state.getTerminals().map((a) => `${a.name}=${a.id}`),
    });

    // If we created sessions, save the updated config with real IDs
    if (createdSessionCount > 0) {
      await saveCurrentConfiguration(state);
      logger.info("Saved config with new session IDs", {
        workspacePath,
        newSessionCount: createdSessionCount,
      });
    }

    // Reconnect agents to existing daemon terminals
    await reconnectTerminals();

    // Auto-create sessions if ALL terminals are missing (clean slate reset scenario)
    await autoCreateSessionsIfAllMissing(workspacePath, deps);

    // Verify terminal sessions health to clear any stale flags
    await verifyTerminalSessions();
  }
}

/**
 * Start autonomous mode for eligible terminals
 *
 * @param state - App state
 */
async function startAutonomousMode(state: AppState): Promise<void> {
  const { getIntervalPromptManager } = await import("./interval-prompt-manager");
  const intervalManager = getIntervalPromptManager();
  intervalManager.startAll(state);
  logger.info("Started interval prompt managers");
}

/**
 * Persist workspace path for next app launch
 *
 * @param workspacePath - Path to workspace
 */
async function persistWorkspacePath(workspacePath: string): Promise<void> {
  try {
    await invoke("set_stored_workspace", { path: workspacePath });
    logger.info("Workspace path stored", { workspacePath });
  } catch (error) {
    logger.error("Failed to store workspace path", error as Error, {
      workspacePath,
    });
    // Non-fatal - workspace is still loaded
  }
}

/**
 * Handle manual workspace path entry
 *
 * This is the main orchestrator that coordinates all workspace lifecycle steps:
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
  try {
    // Step 1-2: Expand and validate path
    const expandedPath = await expandAndValidatePath(path, deps);
    if (!expandedPath) {
      return;
    }

    // Step 3: Ensure repository scaffolding
    await ensureWorkspaceScaffolding(expandedPath);

    // Step 4-8: Check initialization and handle accordingly
    const isInitialized = await invoke<boolean>("check_loom_initialized", { path: expandedPath });
    logger.info("Loom initialization status", { isInitialized, workspacePath: expandedPath });

    if (!isInitialized) {
      // New workspace: initialize, create terminals, launch agents
      await initializeNewWorkspace(expandedPath, deps);
    } else {
      // Existing workspace: load config, reconnect, recover
      await loadExistingWorkspace(expandedPath, deps);
    }

    // Step 9: Start autonomous mode
    await startAutonomousMode(deps.state);

    // Step 10: Set workspace as active
    deps.state.setWorkspace(expandedPath);
    logger.info("Workspace fully loaded", { workspacePath: expandedPath });

    // Step 11: Persist workspace path
    await persistWorkspacePath(expandedPath);
  } catch (error) {
    logger.error("Error handling workspace", error as Error, { path });
    showToast(`Error: ${error}`, "error");
  }
}
