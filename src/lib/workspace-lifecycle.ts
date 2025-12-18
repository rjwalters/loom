import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { loadWorkspaceConfig, saveCurrentConfiguration, setConfigWorkspace } from "./config";
import type { AgentLauncherDependencies, CoreDependencies } from "./dependencies";
import { Logger } from "./logger";
import { createTerminalsWithRetry, type TerminalConfig } from "./parallel-terminal-creator";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";
import { showToast } from "./toast";
import { initializeTerminals } from "./workspace-common";
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
export interface WorkspaceLifecycleDependencies
  extends CoreDependencies,
    AgentLauncherDependencies {
  validateWorkspacePath: (path: string) => Promise<boolean>;
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
  deps.state.workspace.setDisplayedWorkspace(expandedPath);
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
  state.terminals.setNextTerminalNumber(config.nextAgentNumber);

  if (config.agents && config.agents.length > 0) {
    logger.info("Creating terminals for fresh workspace", {
      workspacePath,
      terminalCount: config.agents.length,
    });

    // Use shared terminal initialization logic
    await initializeTerminals({
      workspacePath,
      agents: config.agents,
      state,
      launchAgentsForTerminals,
      // workspace-lifecycle defaults: toastPerFailure=true, others=false
      logPrefix: "workspace-lifecycle",
    });
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
  // Filter agents that need sessions
  const agentsNeedingSessions = config.agents.filter((agent) => agent.id === "__needs_session__");

  if (agentsNeedingSessions.length === 0) {
    return 0;
  }

  logger.info("Creating sessions for migrated terminals", {
    workspacePath,
    count: agentsNeedingSessions.length,
  });

  // Convert to terminal configs for parallel creation
  // Use name as the config ID since all have the same placeholder ID
  const terminalConfigs: TerminalConfig[] = agentsNeedingSessions.map((agent) => ({
    id: agent.name, // Use name as unique identifier for matching results
    name: agent.name,
    role: agent.role || "default",
    workingDir: workspacePath,
    instanceNumber: 0, // Will be assigned by createTerminalsWithRetry
  }));

  // Create sessions in parallel with retry logic
  const { succeeded, failed } = await createTerminalsWithRetry(
    terminalConfigs,
    workspacePath,
    state
  );

  // Update agent IDs for successfully created sessions
  // Match by name since we used name as the config ID
  for (const result of succeeded) {
    const agent = agentsNeedingSessions.find((a) => a.name === result.configId);
    if (agent) {
      agent.id = result.terminalId;
      logger.info("Created session for migrated terminal", {
        workspacePath,
        terminalName: agent.name,
        sessionId: result.terminalId,
      });
    }
  }

  // Log failed session creations
  for (const failure of failed) {
    logger.error("Failed to create session for migrated terminal", failure.error as Error, {
      workspacePath,
      configId: failure.configId,
    });
    // Keep placeholder ID - terminal will show as missing session
    // User can use recovery options
  }

  logger.info("Created sessions for migrated terminals", {
    workspacePath,
    createdCount: succeeded.length,
    failedCount: failed.length,
  });

  return succeeded.length;
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
  const terminals = state.terminals.getTerminals();
  const allMissing = terminals.every((t) => t.missingSession === true);

  if (!allMissing || terminals.length === 0) {
    return 0;
  }

  logger.info("All terminals missing, auto-creating sessions (clean slate reset recovery)", {
    workspacePath,
    terminalCount: terminals.length,
  });

  // Convert terminals to configs for parallel creation
  const terminalConfigs: TerminalConfig[] = terminals.map((terminal) => ({
    id: terminal.id,
    name: terminal.name,
    role: terminal.role || "default",
    workingDir: workspacePath,
    instanceNumber: 0, // Will be assigned by createTerminalsWithRetry
  }));

  // Create sessions in parallel with retry logic
  const { succeeded, failed } = await createTerminalsWithRetry(
    terminalConfigs,
    workspacePath,
    state
  );

  // Update terminal state for successfully created sessions
  for (const result of succeeded) {
    const terminal = terminals.find((t) => t.id === result.configId);
    if (terminal) {
      state.terminals.updateTerminal(terminal.id, {
        id: result.terminalId,
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
      logger.info("Auto-created session for terminal", {
        workspacePath,
        terminalName: terminal.name,
        sessionId: result.terminalId,
      });
    }
  }

  // Log failed session creations
  for (const failure of failed) {
    const terminal = terminals.find((t) => t.id === failure.configId);
    logger.error("Failed to auto-create session for terminal", failure.error as Error, {
      workspacePath,
      terminalName: terminal?.name,
    });
    // Keep in error state - user can try manual recovery
  }

  if (succeeded.length > 0) {
    logger.info("Auto-created sessions", {
      workspacePath,
      createdCount: succeeded.length,
      failedCount: failed.length,
      totalCount: terminals.length,
    });

    // Save updated config with new session IDs
    await saveCurrentConfiguration(state);

    // Launch agents for terminals with role configs
    logger.info("Launching agents for auto-created terminals", {
      workspacePath,
    });
    await launchAgentsForTerminals(workspacePath, state.terminals.getTerminals());
  }

  return succeeded.length;
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
  state.terminals.setNextTerminalNumber(config.nextAgentNumber);

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
    state.terminals.loadTerminals(config.agents);
    logger.info("State after loadAgents", {
      workspacePath,
      terminals: state.terminals.getTerminals().map((a) => `${a.name}=${a.id}`),
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
    deps.state.workspace.setWorkspace(expandedPath);
    logger.info("Workspace fully loaded", { workspacePath: expandedPath });

    // Step 11: Persist workspace path
    await persistWorkspacePath(expandedPath);
  } catch (error) {
    logger.error("Error handling workspace", error as Error, { path });
    showToast(`Error: ${error}`, "error");
  }
}
