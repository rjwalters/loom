import { sendPromptToAgent } from "./agent-launcher";
import { Logger } from "./logger";
import type { AppState, Terminal } from "./state";
import { detectTerminalState, type TerminalStatus } from "./terminal-state-parser";

const logger = Logger.forComponent("interval-prompt-manager");

/**
 * State tracking for responsive interval prompts
 */
interface TerminalIntervalState {
  terminalId: string;
  lastPromptTime: number; // Timestamp of last interval prompt
  minInterval: number; // Minimum time between prompts in ms
  isIdle: boolean; // Current busy/idle state
  previousStatus: TerminalStatus | null; // Previous status for transition detection
  intervalPrompt: string; // Prompt to send on interval
}

/**
 * Responsive interval prompt manager with min-time-between semantics
 *
 * Key features:
 * - Triggers prompts when agent becomes idle AND min interval has elapsed
 * - Periodic polling (10s) to check state and timing
 * - Immediate trigger on busy→idle transitions (no waiting for next poll)
 * - Interval 0: Continuous loop (prompt immediately when idle)
 * - Prevents spam with single-execution protection
 *
 * This replaces fixed-interval timers with state-aware responsive prompts.
 */
class IntervalPromptManager {
  private states: Map<string, TerminalIntervalState> = new Map();
  private activePrompts: Set<string> = new Set();
  private pollInterval: number | null = null;
  private readonly POLL_INTERVAL_MS = 10000; // Check every 10 seconds

  /**
   * Start responsive interval management for a terminal
   *
   * @param terminal - The terminal to manage
   */
  start(terminal: Terminal): void {
    const targetInterval = terminal.roleConfig?.targetInterval as number | undefined;

    // Validate configuration
    if (targetInterval === undefined || targetInterval < 0 || !terminal.roleConfig) {
      logger.warn("Terminal does not have valid interval configuration", {
        terminalId: terminal.id,
        targetInterval,
      });
      return;
    }

    const intervalPrompt = (terminal.roleConfig.intervalPrompt as string) || "Continue working";

    logger.info("Starting responsive interval management", {
      terminalId: terminal.id,
      minInterval: targetInterval,
      intervalPrompt,
      isContinuous: targetInterval === 0,
    });

    // Stop existing state tracking if present
    this.stop(terminal.id);

    // Initialize state
    this.states.set(terminal.id, {
      terminalId: terminal.id,
      lastPromptTime: Date.now(), // Set to now to respect min interval on first check
      minInterval: targetInterval,
      isIdle: false, // Conservative default - assume busy until we check
      previousStatus: null,
      intervalPrompt,
    });

    // Start polling if not already running
    this.startPolling();
  }

  /**
   * Stop responsive interval management for a terminal
   *
   * @param terminalId - The terminal to stop managing
   */
  stop(terminalId: string): void {
    this.states.delete(terminalId);

    // Stop polling if no terminals left
    if (this.states.size === 0) {
      this.stopPolling();
    }

    logger.info("Stopped responsive interval management", { terminalId });
  }

  /**
   * Start periodic state polling
   *
   * @private
   */
  private startPolling(): void {
    if (this.pollInterval !== null) {
      return; // Already polling
    }

    logger.info("Starting state polling", {
      intervalMs: this.POLL_INTERVAL_MS,
      terminalCount: this.states.size,
    });

    this.pollInterval = window.setInterval(() => {
      void this.checkAllTerminals();
    }, this.POLL_INTERVAL_MS);
  }

  /**
   * Stop periodic state polling
   *
   * @private
   */
  private stopPolling(): void {
    if (this.pollInterval !== null) {
      window.clearInterval(this.pollInterval);
      this.pollInterval = null;
      logger.info("Stopped state polling");
    }
  }

  /**
   * Check all terminals for state and timing
   *
   * @private
   */
  private async checkAllTerminals(): Promise<void> {
    for (const [terminalId, state] of this.states) {
      try {
        await this.checkTerminal(terminalId, state);
      } catch (error) {
        logger.error("Failed to check terminal", error as Error, { terminalId });
      }
    }
  }

  /**
   * Check a single terminal for state and timing
   *
   * @param terminalId - Terminal to check
   * @param state - Current interval state
   * @private
   */
  private async checkTerminal(terminalId: string, state: TerminalIntervalState): Promise<void> {
    // Detect current terminal state
    const terminalState = await detectTerminalState(terminalId, 20);

    // Map terminal status to busy/idle
    const isIdle = terminalState.status === "idle" || terminalState.status === "waiting-input";

    // Detect state transitions
    const wasBusy = !state.isIdle;
    const nowIdle = isIdle;
    const transition = wasBusy && nowIdle ? "busy→idle" : null;

    // Update state
    state.isIdle = isIdle;
    const previousStatus = state.previousStatus;
    state.previousStatus = terminalState.status;

    // Log state check
    logger.info("Terminal state check", {
      terminalId,
      status: terminalState.status,
      isIdle,
      transition,
      previousStatus,
    });

    // Check if we should prompt
    if (this.shouldPrompt(state)) {
      logger.info("Triggering interval prompt", {
        terminalId,
        reason: transition || "periodic",
        timeSinceLastPrompt: Date.now() - state.lastPromptTime,
        minInterval: state.minInterval,
      });

      await this.sendPrompt(terminalId, state);
    }
  }

  /**
   * Check if we should send a prompt to this terminal
   *
   * @param state - Terminal interval state
   * @returns true if we should prompt
   * @private
   */
  private shouldPrompt(state: TerminalIntervalState): boolean {
    // Must be idle
    if (!state.isIdle) {
      return false;
    }

    // Must not already have active prompt
    if (this.activePrompts.has(state.terminalId)) {
      return false;
    }

    // Check time since last prompt
    const elapsed = Date.now() - state.lastPromptTime;
    const minIntervalElapsed = elapsed >= state.minInterval;

    if (!minIntervalElapsed) {
      logger.info("Not enough time elapsed since last prompt", {
        terminalId: state.terminalId,
        elapsed,
        minInterval: state.minInterval,
        remaining: state.minInterval - elapsed,
      });
      return false;
    }

    return true;
  }

  /**
   * Send interval prompt to terminal
   *
   * @param terminalId - Terminal to send prompt to
   * @param state - Terminal interval state
   * @private
   */
  private async sendPrompt(terminalId: string, state: TerminalIntervalState): Promise<void> {
    // Mark as active to prevent overlapping executions
    this.activePrompts.add(terminalId);

    try {
      await sendPromptToAgent(terminalId, state.intervalPrompt);

      // Update last prompt time
      state.lastPromptTime = Date.now();

      logger.info("Sent interval prompt", {
        terminalId,
        prompt: state.intervalPrompt,
      });
    } catch (error) {
      logger.error("Failed to send interval prompt", error as Error, {
        terminalId,
      });
    } finally {
      // Always cleanup, even on error
      this.activePrompts.delete(terminalId);
    }
  }

  /**
   * Manually trigger interval prompt immediately
   *
   * This bypasses timing checks and forces a prompt to be sent.
   * Used for manual "Run Now" actions from UI.
   *
   * @param terminal - Terminal to prompt
   */
  async runNow(terminal: Terminal): Promise<void> {
    const state = this.states.get(terminal.id);
    if (!state) {
      logger.warn("Cannot run now - terminal not managed", {
        terminalId: terminal.id,
      });
      return;
    }

    logger.info("Manually triggering interval prompt", {
      terminalId: terminal.id,
    });

    await this.sendPrompt(terminal.id, state);
  }

  /**
   * Check if terminal is being managed
   *
   * @param terminalId - Terminal to check
   * @returns true if terminal is managed
   */
  isManaged(terminalId: string): boolean {
    return this.states.has(terminalId);
  }

  /**
   * Get status for a terminal
   *
   * @param terminalId - Terminal to get status for
   * @returns Terminal interval state or undefined
   */
  getStatus(terminalId: string): TerminalIntervalState | undefined {
    return this.states.get(terminalId);
  }

  /**
   * Get all managed terminals
   *
   * @returns Array of all terminal interval states
   */
  getAllStatus(): TerminalIntervalState[] {
    return Array.from(this.states.values());
  }

  /**
   * Start management for all eligible terminals
   *
   * @param state - Application state
   */
  startAll(state: AppState): void {
    const terminals = state.terminals.getTerminals();

    for (const terminal of terminals) {
      const hasInterval =
        terminal.roleConfig?.targetInterval !== undefined &&
        (terminal.roleConfig.targetInterval as number) >= 0;

      if (hasInterval) {
        this.start(terminal);
      }
    }
  }

  /**
   * Stop all terminal management
   */
  stopAll(): void {
    const terminalIds = Array.from(this.states.keys());
    for (const terminalId of terminalIds) {
      this.stop(terminalId);
    }
  }

  /**
   * Restart management for a terminal (useful after config changes)
   *
   * @param terminal - Terminal to restart
   */
  restart(terminal: Terminal): void {
    this.stop(terminal.id);
    this.start(terminal);
  }
}

// Singleton instance
let intervalPromptManagerInstance: IntervalPromptManager | null = null;

/**
 * Get the singleton interval prompt manager instance
 */
export function getIntervalPromptManager(): IntervalPromptManager {
  if (!intervalPromptManagerInstance) {
    intervalPromptManagerInstance = new IntervalPromptManager();
  }
  return intervalPromptManagerInstance;
}
