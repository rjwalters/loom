import { sendPromptToAgent } from "./agent-launcher";
import { Logger } from "./logger";
import type { AppState, Terminal } from "./state";

const logger = Logger.forComponent("autonomous-manager");

/**
 * Manages autonomous agent intervals for terminals with autonomous mode enabled
 *
 * This module maintains a registry of active interval timers and provides
 * methods to start, stop, and manage autonomous operation for terminals.
 *
 * IMPORTANT: This manager uses terminal ID (stable) as the key for intervals,
 * ensuring that autonomous mode survives daemon restarts.
 */

interface AutonomousInterval {
  terminalId: string; // Stable terminal ID (survives daemon restarts)
  intervalId: number;
  targetInterval: number;
  lastRun: number;
}

class AutonomousManager {
  private intervals: Map<string, AutonomousInterval> = new Map();

  /**
   * Start autonomous mode for a terminal
   *
   * @param terminal - The terminal to start autonomous mode for
   */
  startAutonomous(terminal: Terminal): void {
    // Ensure terminal has autonomous configuration
    const targetInterval = terminal.roleConfig?.targetInterval as number | undefined;
    if (!targetInterval || targetInterval <= 0 || !terminal.roleConfig) {
      logger.warn("Terminal does not have valid autonomous configuration", {
        terminalId: terminal.id,
      });
      return;
    }

    // Stop existing interval if running (use configId)
    this.stopAutonomous(terminal.id);

    const intervalPrompt = (terminal.roleConfig.intervalPrompt as string) || "Continue working";

    logger.info("Starting autonomous mode", {
      terminalId: terminal.id,
      interval: targetInterval,
      intervalPrompt,
    });

    // Set up interval to send prompts
    const intervalId = window.setInterval(async () => {
      try {
        // Use sessionId for IPC call to send prompt
        await sendPromptToAgent(terminal.id, intervalPrompt);
        logger.info("Sent autonomous prompt", { terminalId: terminal.id });

        // Update last run timestamp in the interval record (use configId for lookup)
        const interval = this.intervals.get(terminal.id);
        if (interval) {
          interval.lastRun = Date.now();
        }
      } catch (error) {
        logger.error("Failed to send autonomous prompt", error, {
          terminalId: terminal.id,
        });
      }
    }, targetInterval);

    // Store interval info (use terminal ID as key)
    this.intervals.set(terminal.id, {
      terminalId: terminal.id,
      intervalId,
      targetInterval,
      lastRun: Date.now(),
    });
  }

  /**
   * Stop autonomous mode for a terminal
   *
   * @param terminalId - The terminal ID to stop autonomous mode for
   */
  stopAutonomous(terminalId: string): void {
    const interval = this.intervals.get(terminalId);
    if (interval) {
      logger.info("Stopping autonomous mode", { terminalId });
      window.clearInterval(interval.intervalId);
      this.intervals.delete(terminalId);
    }
  }

  /**
   * Check if a terminal has autonomous mode running
   *
   * @param terminalId - The terminal ID to check
   * @returns true if autonomous mode is active
   */
  isAutonomous(terminalId: string): boolean {
    return this.intervals.has(terminalId);
  }

  /**
   * Restart autonomous mode for a terminal (useful after config changes)
   *
   * @param terminal - The terminal to restart autonomous mode for
   */
  restartAutonomous(terminal: Terminal): void {
    this.stopAutonomous(terminal.id);
    this.startAutonomous(terminal);
  }

  /**
   * Start autonomous mode for all eligible terminals in state
   *
   * This should be called on app startup to restore autonomous agents
   *
   * @param state - The application state
   */
  startAllAutonomous(state: AppState): void {
    const terminals = state.getTerminals();

    for (const terminal of terminals) {
      // Check if terminal has role with autonomous enabled
      const hasRole = terminal.role !== undefined;
      const hasInterval =
        terminal.roleConfig?.targetInterval !== undefined &&
        (terminal.roleConfig.targetInterval as number) > 0;

      if (hasRole && hasInterval) {
        this.startAutonomous(terminal);
      }
    }
  }

  /**
   * Stop all autonomous intervals
   *
   * This should be called on app shutdown
   */
  stopAll(): void {
    logger.info("Stopping all autonomous intervals", { count: this.intervals.size });
    for (const [terminalId] of this.intervals) {
      this.stopAutonomous(terminalId);
    }
  }

  /**
   * Get autonomous status for a terminal
   *
   * @param terminalId - The terminal ID to get status for
   * @returns The interval info or undefined if not autonomous
   */
  getStatus(terminalId: string): AutonomousInterval | undefined {
    return this.intervals.get(terminalId);
  }

  /**
   * Get all active autonomous intervals
   *
   * @returns Array of all autonomous interval info
   */
  getAllStatus(): AutonomousInterval[] {
    return Array.from(this.intervals.values());
  }

  /**
   * Manually trigger the interval prompt for a terminal immediately
   *
   * This executes the terminal's interval prompt and resets the interval timer,
   * allowing users to trigger autonomous work on-demand without waiting for
   * the next scheduled interval.
   *
   * @param terminal - The terminal to execute the prompt for
   * @returns Promise that resolves when the prompt is sent
   */
  async runNow(terminal: Terminal): Promise<void> {
    // Ensure terminal has autonomous configuration
    const targetInterval = terminal.roleConfig?.targetInterval as number | undefined;
    if (!targetInterval || targetInterval <= 0 || !terminal.roleConfig) {
      logger.warn("Terminal does not have valid autonomous configuration", {
        terminalId: terminal.id,
      });
      return;
    }

    const intervalPrompt = (terminal.roleConfig.intervalPrompt as string) || "Continue working";

    logger.info("Manually executing interval prompt", {
      terminalId: terminal.id,
      intervalPrompt,
    });

    try {
      // Send the prompt to the agent
      await sendPromptToAgent(terminal.id, intervalPrompt);
      logger.info("Manually sent prompt", { terminalId: terminal.id });

      // Reset the interval timer by restarting autonomous mode
      // This ensures the next automatic execution happens targetInterval ms from now
      this.restartAutonomous(terminal);
      logger.info("Reset interval timer", { terminalId: terminal.id });
    } catch (error) {
      logger.error("Failed to manually execute prompt", error, {
        terminalId: terminal.id,
      });
      throw error;
    }
  }
}

// Singleton instance
let autonomousManagerInstance: AutonomousManager | null = null;

/**
 * Get the singleton autonomous manager instance
 */
export function getAutonomousManager(): AutonomousManager {
  if (!autonomousManagerInstance) {
    autonomousManagerInstance = new AutonomousManager();
  }
  return autonomousManagerInstance;
}
