import { invoke } from "@tauri-apps/api/core";

import { appLevelState } from "./app-state";
import { Logger } from "./logger";
import { getOutputPoller } from "./output-poller";
import { type AppState, TerminalStatus } from "./state";
import { getTerminalManager } from "./terminal-manager";

const logger = Logger.forComponent("terminal-display");

/**
 * Track which terminals have had their health checked.
 * This prevents redundant health checks during the 1-second render loop (Phase 3: Debouncing).
 */
export const healthCheckedTerminals = new Set<string>();

/**
 * Clear the health-checked terminals set.
 * Called when workspace closes or engine restarts.
 */
export function clearHealthCheckedTerminals(): void {
  const previousSize = healthCheckedTerminals.size;
  healthCheckedTerminals.clear();
  logger.info("Cleared health-checked terminals set", {
    previousSize,
    currentSize: healthCheckedTerminals.size,
  });
}

/**
 * Initialize xterm.js terminal display for a given terminal.
 * Handles session health checking, terminal creation, and show/hide logic.
 */
export async function initializeTerminalDisplay(
  terminalId: string,
  state: AppState
): Promise<void> {
  const containerId = `xterm-container-${terminalId}`;
  const terminalManager = getTerminalManager();
  const outputPoller = getOutputPoller();

  // Skip placeholder IDs - they're already broken and will show error UI
  if (terminalId === "__unassigned__") {
    logger.warn("Skipping placeholder terminal ID", { terminalId });
    return;
  }

  // Phase 3: Skip health check if already checked (debouncing)
  if (healthCheckedTerminals.has(terminalId)) {
    logger.info("Terminal already health-checked, skipping redundant check", {
      terminalId,
      setSize: healthCheckedTerminals.size,
    });
    // Continue with xterm initialization without re-checking
  } else {
    // Check session health before initializing
    try {
      logger.info("Performing new health check for terminal", {
        terminalId,
        setSizeBefore: healthCheckedTerminals.size,
      });
      const hasSession = await invoke<boolean>("check_session_health", { id: terminalId });
      logger.info("Session health check result", {
        terminalId,
        hasSession,
      });

      if (!hasSession) {
        logger.warn("Terminal has no tmux session", { terminalId });

        // Mark terminal as having missing session (only if not already marked)
        const terminal = state.terminals.getTerminal(terminalId);
        logger.info("Terminal state before update", {
          terminalId,
          missingSession: terminal?.missingSession,
        });
        if (terminal && !terminal.missingSession) {
          logger.info("Setting missingSession=true for terminal", { terminalId });
          state.terminals.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        }

        // Add to checked set even for failures to prevent repeated checks
        healthCheckedTerminals.add(terminalId);
        logger.info("Added terminal to health-checked set (failed check)", {
          terminalId,
          setSize: healthCheckedTerminals.size,
        });
        return; // Don't create xterm instance - error UI will show instead
      }

      logger.info("Session health check passed, proceeding with xterm initialization", {
        terminalId,
      });

      // Add to checked set after successful health check (Phase 3: Debouncing)
      healthCheckedTerminals.add(terminalId);
      logger.info("Added terminal to health-checked set (passed check)", {
        terminalId,
        setSize: healthCheckedTerminals.size,
      });
    } catch (error) {
      logger.error("Failed to check session health", error, { terminalId });
      // Add to checked set even on error to prevent retry spam
      healthCheckedTerminals.add(terminalId);
      logger.info("Added terminal to health-checked set (error during check)", {
        terminalId,
        setSize: healthCheckedTerminals.size,
      });
      // Continue anyway - better to try than not
    }
  }

  // Check if terminal already exists
  const existingManaged = terminalManager.getTerminal(terminalId);
  if (existingManaged) {
    // Terminal exists - just show/hide as needed
    logger.info("Terminal already exists, using show/hide", { terminalId });

    // Hide previous terminal (if different)
    const currentAttachedTerminalId = appLevelState.currentAttachedTerminalId;
    if (currentAttachedTerminalId && currentAttachedTerminalId !== terminalId) {
      logger.info("Hiding previous terminal", {
        terminalId: currentAttachedTerminalId,
      });
      terminalManager.hideTerminal(currentAttachedTerminalId);
      outputPoller.pausePolling(currentAttachedTerminalId);
    }

    // Show current terminal
    logger.info("Showing terminal", { terminalId });
    terminalManager.showTerminal(terminalId);

    // Resume polling for current terminal
    logger.info("Resuming polling for terminal", { terminalId });
    outputPoller.resumePolling(terminalId);

    appLevelState.currentAttachedTerminalId = terminalId;
    return;
  }

  // Terminal doesn't exist yet - create it
  logger.info("Creating new terminal", { terminalId });

  // Wait for DOM to be ready
  setTimeout(() => {
    // Hide all other terminals first
    const currentAttachedTerminalId = appLevelState.currentAttachedTerminalId;
    if (currentAttachedTerminalId) {
      terminalManager.hideTerminal(currentAttachedTerminalId);
      outputPoller.pausePolling(currentAttachedTerminalId);
    }

    // Create new terminal (will be shown by default in createTerminal)
    const managed = terminalManager.createTerminal(terminalId, containerId);
    if (managed) {
      // Show this terminal
      terminalManager.showTerminal(terminalId);

      // Start polling for output
      outputPoller.startPolling(terminalId);
      appLevelState.currentAttachedTerminalId = terminalId;

      logger.info("Created and showing terminal", { terminalId });
    }
  }, 0);
}
