import { invoke } from "@tauri-apps/api/tauri";
import type { AppState, Terminal } from "./state";
import { TerminalStatus } from "./state";

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

  console.log("[launchAgentsForTerminals] Launching agents for configured terminals");
  console.log("[launchAgentsForTerminals] workspacePath:", workspacePath);
  console.log(
    "[launchAgentsForTerminals] terminals:",
    terminals.map((t) => `${t.name}=${t.id}, role=${t.role}`)
  );

  // Filter terminals that have Claude Code worker role
  const workersToLaunch = terminals.filter(
    (t) => t.role === "claude-code-worker" && t.roleConfig && t.roleConfig.roleFile
  );

  console.log(
    `[launchAgentsForTerminals] Found ${workersToLaunch.length} terminals with role configs`,
    workersToLaunch.map((t) => `${t.name}=${t.id}`)
  );

  // Track terminals that were successfully launched
  const launchedTerminalIds: string[] = [];

  // Launch each worker
  for (const terminal of workersToLaunch) {
    try {
      const roleConfig = terminal.roleConfig;
      if (!roleConfig || !roleConfig.roleFile) {
        continue;
      }

      console.log(`[launchAgentsForTerminals] Launching ${terminal.name} (${terminal.id})`);

      // Set terminal to busy status BEFORE launching agent
      // This prevents HealthMonitor from incorrectly marking it as missing during the launch process
      state.updateTerminal(terminal.id, { status: TerminalStatus.Busy });
      console.log(`[launchAgentsForTerminals] Set ${terminal.name} to busy status`);

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
      } else if (workerType === "codex") {
        // Codex with worktree support (optional - starts in main workspace if empty)
        console.log(`[launchAgentsForTerminals] Launching Codex for ${terminal.name}...`);
        const { launchCodexAgent } = await import("./agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        console.log(
          `[launchAgentsForTerminals] Launching Codex agent for ${terminal.name} (id=${terminal.id}) in ${locationDesc}...`
        );

        // Launch Codex agent (will use main workspace if worktreePath is empty)
        await launchCodexAgent(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        console.log(`[launchAgentsForTerminals] Codex agent launched in ${locationDesc}`);
      } else {
        // Claude with worktree support (optional - starts in main workspace if empty)
        console.log(`[launchAgentsForTerminals] Importing agent-launcher for ${terminal.name}...`);
        const { launchAgentInTerminal } = await import("./agent-launcher");

        // Use worktree path if available, otherwise main workspace
        const locationDesc = terminal.worktreePath
          ? `worktree ${terminal.worktreePath}`
          : "main workspace";
        console.log(
          `[launchAgentsForTerminals] Launching agent for ${terminal.name} (id=${terminal.id}) in ${locationDesc}...`
        );

        // Launch agent (will use main workspace if worktreePath is empty)
        await launchAgentInTerminal(
          terminal.id,
          roleConfig.roleFile as string,
          workspacePath,
          terminal.worktreePath || ""
        );

        console.log(`[launchAgentsForTerminals] Agent launched in ${locationDesc}`);
      }

      console.log(`[launchAgentsForTerminals] Successfully launched ${terminal.name}`);

      // Track successfully launched terminals (will reset to idle AFTER all launches complete)
      launchedTerminalIds.push(terminal.id);
    } catch (error) {
      const errorMessage = `Failed to launch agent for ${terminal.name}: ${error}`;
      console.error(`[launchAgentsForTerminals] ${errorMessage}`);

      // Still track this terminal ID - reset to idle after all launches complete
      // (agent launch failed but terminal exists and should not stay in busy state forever)
      launchedTerminalIds.push(terminal.id);

      // Show error to user so they know what failed
      alert(errorMessage);

      // Continue with other terminals even if one fails
    }
  }

  // Reset ALL launched terminals to idle status AFTER all launches complete
  // This prevents the periodic HealthMonitor (30s interval) from catching terminals in idle
  // state before all agent launches finish (which can take 2+ minutes for 6 terminals)
  console.log(
    `[launchAgentsForTerminals] All agent launches complete, resetting ${launchedTerminalIds.length} terminals to idle`
  );
  for (const terminalId of launchedTerminalIds) {
    state.updateTerminal(terminalId, { status: TerminalStatus.Idle });
    console.log(`[launchAgentsForTerminals] Reset ${terminalId} to idle status`);
  }

  console.log("[launchAgentsForTerminals] Agent launch complete");
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

  console.log(`[verifyTerminalSessions] Checking health for ${terminals.length} terminals...`);

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
      console.error(`[verifyTerminalSessions] Failed to check ${terminal.id}:`, error);
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
      console.log(`[verifyTerminalSessions] Clearing stale missingSession flag for ${terminal.id}`);
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
      clearedCount++;
    } else if (!hasSession && !terminal.missingSession) {
      // Mark as missing if not already marked
      console.log(`[verifyTerminalSessions] Marking ${terminal.id} as missing session`);
      state.updateTerminal(terminal.id, {
        status: TerminalStatus.Error,
        missingSession: true,
      });
      markedMissingCount++;
    }
  }

  console.log(
    `[verifyTerminalSessions] Verification complete: ${clearedCount} cleared, ${markedMissingCount} marked missing`
  );
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

  console.log("[reconnectTerminals] Querying daemon for active terminals...");

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
    console.log(`[reconnectTerminals] Found ${daemonTerminals.length} active daemon terminals`);

    // Create a set of active terminal IDs for quick lookup
    const activeTerminalIds = new Set(daemonTerminals.map((t) => t.id));

    // Get all agents from state
    const agents = state.getTerminals();
    console.log(`[reconnectTerminals] Config has ${agents.length} agents`);

    let reconnectedCount = 0;
    let missingCount = 0;

    // For each agent in config, check if daemon has it
    for (const agent of agents) {
      // Check if agent has placeholder ID (shouldn't happen after proper initialization)
      if (agent.id === "__unassigned__") {
        console.log(
          `[reconnectTerminals] Agent ${agent.name} has placeholder ID, skipping (already in error state)`
        );

        // Don't call state.updateTerminal() here - it triggers infinite render loop
        // The terminal already shows as missing because check_session_health will fail for "__unassigned__"
        missingCount++;
        continue;
      }

      if (activeTerminalIds.has(agent.id)) {
        console.log(`[reconnectTerminals] Reconnecting agent ${agent.name} (${agent.id})`);

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
        console.log(
          `[reconnectTerminals] Agent ${agent.name} (${agent.id}) not found in daemon, marking as missing`
        );

        // Mark terminal as having missing session so user can see it needs recovery (use configId for state)
        state.updateTerminal(agent.id, {
          status: TerminalStatus.Error,
          missingSession: true,
        });

        missingCount++;
      }
    }

    console.log(
      `[reconnectTerminals] Reconnection complete: ${reconnectedCount} reconnected, ${missingCount} missing`
    );

    // If we reconnected at least some terminals, save the updated state
    if (reconnectedCount > 0 && saveCurrentConfig) {
      await saveCurrentConfig();
    }
  } catch (error) {
    console.error("[reconnectTerminals] Failed to reconnect terminals:", error);
    // Non-fatal - workspace is still loaded
    alert(
      `Warning: Could not reconnect to daemon terminals.\n\n` +
        `Error: ${error}\n\n` +
        `Terminals may need to be recreated. Check Help → Daemon Status for more info.`
    );
  }
}
