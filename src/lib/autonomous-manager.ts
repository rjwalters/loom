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
  private activePrompts: Set<string> = new Set();

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

    // Stop existing interval if running (don't await - start new interval immediately)
    void this.stopAutonomous(terminal.id);

    const intervalPrompt = (terminal.roleConfig.intervalPrompt as string) || "Continue working";

    logger.info("Starting autonomous mode", {
      terminalId: terminal.id,
      interval: targetInterval,
      intervalPrompt,
    });

    // Set up interval to send prompts with overrun protection
    const intervalId = window.setInterval(() => {
      void this.executeWithOverrunProtection(terminal.id, intervalPrompt);
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
   * Execute prompt with overlap prevention
   *
   * Prevents multiple simultaneous executions of the same terminal's prompt.
   * If a prompt is already executing for the terminal, this method logs a
   * warning and returns immediately.
   *
   * @param terminalId - The terminal ID to execute the prompt for
   * @param intervalPrompt - The prompt to send to the agent
   * @private
   */
  private async executeWithOverrunProtection(
    terminalId: string,
    intervalPrompt: string
  ): Promise<void> {
    // Check if already executing
    if (this.activePrompts.has(terminalId)) {
      logger.warn("Skipping overlapping execution", {
        terminalId,
        message: "Previous execution still in progress",
      });
      return;
    }

    // Mark as active
    this.activePrompts.add(terminalId);

    try {
      // Send the prompt to the agent
      await sendPromptToAgent(terminalId, intervalPrompt);
      logger.info("Sent autonomous prompt", { terminalId });

      // Update last run timestamp
      const interval = this.intervals.get(terminalId);
      if (interval) {
        interval.lastRun = Date.now();
      }
    } catch (error) {
      logger.error("Failed to send autonomous prompt", error, {
        terminalId,
      });
    } finally {
      // Always cleanup, even on error
      this.activePrompts.delete(terminalId);
    }
  }

  /**
   * Stop autonomous mode for a terminal
   *
   * Waits for any active execution to complete before returning.
   *
   * @param terminalId - The terminal ID to stop autonomous mode for
   * @returns Promise that resolves when autonomous mode is stopped
   */
  async stopAutonomous(terminalId: string): Promise<void> {
    const interval = this.intervals.get(terminalId);
    if (interval) {
      logger.info("Stopping autonomous mode", { terminalId });
      window.clearInterval(interval.intervalId);
      this.intervals.delete(terminalId);

      // Wait for active execution to finish
      while (this.activePrompts.has(terminalId)) {
        logger.info("Waiting for active execution to complete", { terminalId });
        await new Promise((resolve) => setTimeout(resolve, 100));
      }

      logger.info("Autonomous mode stopped", { terminalId });
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
   * @returns Promise that resolves when restart is complete
   */
  async restartAutonomous(terminal: Terminal): Promise<void> {
    await this.stopAutonomous(terminal.id);
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
   * This should be called on app shutdown. Waits for all active
   * executions to complete before returning.
   *
   * @returns Promise that resolves when all intervals are stopped
   */
  async stopAll(): Promise<void> {
    logger.info("Stopping all autonomous intervals", { count: this.intervals.size });
    const terminalIds = Array.from(this.intervals.keys());
    await Promise.all(terminalIds.map((id) => this.stopAutonomous(id)));
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
   * Uses overrun protection to prevent overlapping with automatic executions.
   *
   * @param terminal - The terminal to execute the prompt for
   * @returns Promise that resolves when the prompt is sent and interval is reset
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

    // Execute with overrun protection
    await this.executeWithOverrunProtection(terminal.id, intervalPrompt);

    // Reset the interval timer by restarting autonomous mode
    // This ensures the next automatic execution happens targetInterval ms from now
    await this.restartAutonomous(terminal);
    logger.info("Reset interval timer", { terminalId: terminal.id });
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
