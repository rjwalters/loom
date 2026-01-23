import { invoke } from "@tauri-apps/api/core";
import { loadConfig } from "./config";
import { Logger } from "./logger";
import { startOfflineScheduler } from "./offline-scheduler";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";
import { showToast } from "./toast";

const logger = Logger.forComponent("terminal-lifecycle");

/**
 * Terminal lifecycle management
 *
 * This module handles terminal lifecycle operations:
 * - Launching agents for terminals with role configurations
 * - Reconnecting terminals to daemon after config load
 * - Verifying terminal session health
 */

/**
 * Dependencies for terminal lifecycle operations
 */
export interface TerminalLifecycleDependencies {
  state: AppState;
  initializeTerminalDisplay?: (terminalId: string) => void;
  saveCurrentConfig?: () => Promise<void>;
}

/**
 * Launch agents for terminals that have role configurations
 *
 * This function is called after workspace initialization or factory reset
 * to automatically start Claude agents for terminals with roleConfig.
 *
 * @param workspacePath - The workspace directory path
 * @param terminals - Array of terminal configurations
 * @param deps - Injected dependencies
 */
export async function launchAgentsForTerminals(
  workspacePath: string,
  terminals: Terminal[],
  deps: TerminalLifecycleDependencies
): Promise<void> {
  const { state } = deps;

  logger.info("Launching agents for configured terminals", {
    workspacePath,
    terminals: terminals.map((t) => `${t.name}=${t.id}, role=${t.role}`),
  });

  // Check if offline mode is enabled
  const config = await loadConfig();
  if (config.offlineMode) {
    logger.info("Offline mode enabled, skipping agent launches", {
      workspacePath,
      terminalCount: terminals.length,
    });

    // Set all terminals to idle status (they're already created, just not running agents)
    for (const terminal of terminals) {
      state.updateTerminal(terminal.id, { status: TerminalStatus.Idle });
    }

    logger.info("All terminals set to idle (offline mode)", {
      workspacePath,
      terminalCount: terminals.length,
    });

    // Start offline scheduler to send periodic status echoes
    startOfflineScheduler(terminals, workspacePath);

    logger.info("Offline mode setup complete", { workspacePath });
    return;
  }

  // Filter terminals that have Claude Code worker role
  const workersToLaunch = terminals.filter(
    (t) => t.role === "claude-code-worker" && t.roleConfig && t.roleConfig.roleFile
  );

  logger.info("Found terminals with role configs", {
    workspacePath,
    workerCount: workersToLaunch.length,
    workers: workersToLaunch.map((t) => `${t.name}=${t.id}`),
  });

  // Track terminals that were successfully launched
  const launchedTerminalIds: string[] = [];

  // Launch each worker
  for (const terminal of workersToLaunch) {
    try {
      const roleConfig = terminal.roleConfig;
      if (!roleConfig || !roleConfig.roleFile) {
        continue;
      }

      logger.info("Launching agent", {
        workspacePath,
        terminalName: terminal.name,
        terminalId: terminal.id,
      });

      // Set terminal to busy status BEFORE launching agent
      // This prevents HealthMonitor from incorrectly marking it as missing during the launch process
      state.updateTerminal(terminal.id, { status: TerminalStatus.Busy });
      logger.info("Set terminal to busy status", {
        workspacePath,
        terminalName: terminal.name,
        terminalId: terminal.id,
      });

      // Get worker type from config (default to claude)
      const workerType = (roleConfig.workerType as string) || "claude";

      // Launch based on worker type
      if (workerType === "github-copilot") {
        const { launchGitHubCopilotAgent } = await import("./agent-launcher");
        await launchGitHubCopilotAgent(terminal.id);
      } else if (workerType === "gemini") {
        const { launchGeminiCLIAgent } = await import("./agent-launcher");
        await launchGeminiCLIAgent(terminal.id);
      } else if (workerType === "deepseek") {
        const { launchDeepSeekAgent } = await import("./agent-launcher");
        await launchDeepSeekAgent(terminal.id);
      } else if (workerType === "grok") {
        const { launchGrokAgent } = await import("./agent-launcher");
        await launchGrokAgent(terminal.id);
      } else if (workerType === "amp") {
        const { launchAmpAgent } = await import("./agent-launcher");
        await launchAmpAgent(terminal.id);
      } else if (workerType === "codex") {
        // Codex with worktree support (optional - starts in main workspace if empty)
        const { launchCodexAgent } = await import("./agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        logger.info("Launching Codex agent", {
          workspacePath,
          terminalName: terminal.name,
          terminalId: terminal.id,
          location: locationDesc,
        });

        // Launch Codex agent (will use main workspace if worktreePath is empty)
        await launchCodexAgent(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        logger.info("Codex agent launched", {
          workspacePath,
          terminalName: terminal.name,
          location: locationDesc,
        });
      } else {
        // Claude with worktree support (optional - starts in main workspace if empty)
        const { launchAgentInTerminal } = await import("./agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        logger.info("Launching Claude agent", {
          workspacePath,
          terminalName: terminal.name,
          terminalId: terminal.id,
          location: locationDesc,
        });

        // Launch agent (will use main workspace if worktreePath is empty)
        await launchAgentInTerminal(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        logger.info("Claude agent launched", {
          workspacePath,
          terminalName: terminal.name,
          location: locationDesc,
        });
      }

      logger.info("Successfully launched agent", {
        workspacePath,
        terminalName: terminal.name,
        terminalId: terminal.id,
      });

      // Track successfully launched terminals (will reset to idle AFTER all launches complete)
      launchedTerminalIds.push(terminal.id);
    } catch (error) {
      const errorMessage = `Failed to launch agent for ${terminal.name}: ${error}`;
      logger.error("Failed to launch agent", error as Error, {
        workspacePath,
        terminalName: terminal.name,
        terminalId: terminal.id,
      });

      // Still track this terminal ID - reset to idle after all launches complete
      // (agent launch failed but terminal exists and should not stay in busy state forever)
      launchedTerminalIds.push(terminal.id);

      // Show error to user so they know what failed
      showToast(errorMessage, "error", 5000);

      // Continue with other terminals even if one fails
    }
  }

  // Reset ALL launched terminals to idle status AFTER all launches complete
  // This prevents the periodic HealthMonitor (30s interval) from catching terminals in idle
  // state before all agent launches finish (which can take 2+ minutes for 6 terminals)
  logger.info("All agent launches complete, resetting terminals to idle", {
    workspacePath,
    terminalCount: launchedTerminalIds.length,
  });
  for (const terminalId of launchedTerminalIds) {
    state.updateTerminal(terminalId, { status: TerminalStatus.Idle });
    logger.info("Reset terminal to idle status", {
      workspacePath,
      terminalId,
    });
  }

  logger.info("Agent launch complete", { workspacePath });
}

/**
 * Verify terminal sessions health BEFORE rendering
 * This prevents false positives from stale missingSession flags
 * by batch-checking all terminals and updating state synchronously
 *
 * @param deps - Injected dependencies
 */
export async function verifyTerminalSessions(deps: TerminalLifecycleDependencies): Promise<void> {
  const { state } = deps;
  const terminals = state.getTerminals();

  if (terminals.length === 0) {
    return;
  }

  logger.info("Checking terminal session health", {
    terminalCount: terminals.length,
  });

  // Batch check all terminals in parallel
  const checks = terminals.map(async (terminal) => {
    // Skip placeholder IDs
    if (terminal.id === "__unassigned__" || terminal.id === "__needs_session__") {
      return { terminal, hasSession: false };
    }

    try {
      const hasSession = await invoke<boolean>("check_session_health", { id: terminal.id });
      return { terminal, hasSession };
    } catch (error) {
      logger.error("Failed to check terminal session health", error as Error, {
        terminalId: terminal.id,
      });
      return { terminal, hasSession: false };
    }
  });

  const results = await Promise.all(checks);

  // Update state for all terminals based on actual session health
  let clearedCount = 0;
  let markedMissingCount = 0;

  for (const { terminal, hasSession } of results) {
    if (hasSession && terminal.missingSession) {
      // Clear stale missingSession flag
      logger.info("Clearing stale missingSession flag", {
        terminalId: terminal.id,
      });
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
      clearedCount++;
    } else if (!hasSession && !terminal.missingSession) {
      // Mark as missing if not already marked
      logger.info("Marking terminal as missing session", {
        terminalId: terminal.id,
      });
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Error,
        missingSession: true,
      });
      markedMissingCount++;
    }
  }

  logger.info("Terminal session verification complete", {
    clearedCount,
    markedMissingCount,
  });
}

/**
 * Reconnect terminals to daemon after loading config
 *
 * This function queries the daemon for active terminals and reconnects
 * terminals in the config to their corresponding daemon sessions.
 *
 * @param deps - Injected dependencies
 */
export async function reconnectTerminals(deps: TerminalLifecycleDependencies): Promise<void> {
  const { state, initializeTerminalDisplay, saveCurrentConfig } = deps;

  logger.info("Querying daemon for active terminals");

  try {
    // Get list of active terminals from daemon
    interface DaemonTerminalInfo {
      id: string;
      name: string;
      tmux_session: string;
      working_dir: string | null;
      created_at: number;
    }

    const daemonTerminals = await invoke<DaemonTerminalInfo[]>("list_terminals");
    logger.info("Found active daemon terminals", {
      daemonTerminalCount: daemonTerminals.length,
    });

    // Create a set of active terminal IDs for quick lookup
    const activeTerminalIds = new Set(daemonTerminals.map((t) => t.id));

    // Get all agents from state
    const agents = state.getTerminals();
    logger.info("Config agents loaded", {
      agentCount: agents.length,
    });

    let reconnectedCount = 0;
    let missingCount = 0;

    // For each agent in config, check if daemon has it
    for (const agent of agents) {
      // Check if agent has placeholder ID (shouldn't happen after proper initialization)
      if (agent.id === "__unassigned__") {
        logger.info("Agent has placeholder ID, skipping (already in error state)", {
          terminalName: agent.name,
        });

        // Don't call state.updateTerminal() here - it triggers infinite render loop
        // The terminal already shows as missing because check_session_health will fail for "__unassigned__"
        missingCount++;
        continue;
      }

      if (activeTerminalIds.has(agent.id)) {
        logger.info("Reconnecting agent", {
          terminalName: agent.name,
          terminalId: agent.id,
        });

        // Clear any error state from previous connection issues (use configId for state)
        if (agent.missingSession) {
          state.updateTerminal(agent.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        }

        // Initialize xterm for this terminal (will fetch full history)
        // Only initialize if this is the primary terminal to avoid creating too many instances
        if (agent.isPrimary && initializeTerminalDisplay) {
          initializeTerminalDisplay(agent.id);
        }

        reconnectedCount++;
      } else {
        logger.info("Agent not found in daemon, marking as missing", {
          terminalName: agent.name,
          terminalId: agent.id,
        });

        // Mark terminal as having missing session so user can see it needs recovery (use configId for state)
        state.updateTerminal(agent.id, {
          status: TerminalStatus.Error,
          missingSession: true,
        });

        missingCount++;
      }
    }

    logger.info("Reconnection complete", {
      reconnectedCount,
      missingCount,
    });

    // If we reconnected at least some terminals, save the updated state
    if (reconnectedCount > 0 && saveCurrentConfig) {
      await saveCurrentConfig();
    }
  } catch (error) {
    logger.error("Failed to reconnect terminals", error as Error);
    // Non-fatal - workspace is still loaded
    showToast(
      `Warning: Could not reconnect to daemon terminals. Error: ${error}. Terminals may need to be recreated.`,
      "error",
      7000
    );
  }
}
