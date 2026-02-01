/**
 * SessionManager - Manages a single Claude Code tmux session
 *
 * This is a thin wrapper designed for the single-session model where Loom
 * manages exactly one Claude Code instance. It uses the existing terminal IPC
 * infrastructure but provides a simplified API.
 *
 * The SessionManager is Claude Code-specific - no multi-platform abstraction,
 * no workerType config. It hardcodes Claude Code assumptions.
 */

import { invoke } from "@tauri-apps/api/core";
import { CircuitOpenError, getDaemonCircuitBreaker } from "./circuit-breaker";
import { Logger } from "./logger";

const logger = Logger.forComponent("session-manager");

/**
 * Session status information
 */
export interface SessionStatus {
  /** Whether the session exists (tmux session is alive) */
  exists: boolean;
  /** Current session state */
  state: "idle" | "working" | "error" | "stopped";
  /** The tmux session name */
  sessionName: string;
}

/**
 * Output result from getting session output
 */
interface SessionOutput {
  output: string;
  byte_count: number;
}

/**
 * SessionManager - Manages exactly one Claude Code session
 *
 * Uses the existing daemon IPC infrastructure (create_terminal, destroy_terminal,
 * send_terminal_input, get_terminal_output) but provides a simplified API for
 * the single-session model.
 */
export class SessionManager {
  /** Fixed session ID - only one session exists */
  private static readonly SESSION_ID = "claude-session";

  /** Fixed tmux session name */
  private static readonly TMUX_SESSION = "loom-claude-session";

  /** Current session state */
  private sessionState: "idle" | "working" | "error" | "stopped" = "stopped";

  /** Whether the session exists */
  private sessionExists = false;

  /** Last known byte count for incremental output polling */
  private lastByteCount = 0;

  /** Workspace path for the session */
  private workspacePath: string | null = null;

  /**
   * Launch the Claude Code session
   *
   * Creates a tmux session and launches Claude Code CLI in it.
   * This uses the existing daemon IPC - no backend changes needed.
   *
   * @param workspacePath - The workspace directory path
   * @throws Error if session creation fails
   */
  async launchSession(workspacePath: string): Promise<void> {
    if (this.sessionExists) {
      logger.warn("Session already exists, destroying before relaunch");
      await this.destroySession();
    }

    logger.info("Launching Claude Code session", { workspacePath });

    const circuitBreaker = getDaemonCircuitBreaker();

    try {
      // Create the terminal via existing IPC
      await circuitBreaker.execute(async () => {
        await invoke("create_terminal", {
          configId: SessionManager.SESSION_ID,
          name: "Claude Code",
          workingDir: workspacePath,
          role: "claude-code-worker",
          instanceNumber: 0,
        });
      });

      this.workspacePath = workspacePath;
      this.sessionExists = true;
      this.sessionState = "idle";
      this.lastByteCount = 0;

      logger.info("Claude Code session created", {
        sessionId: SessionManager.SESSION_ID,
        workspacePath,
      });

      // Launch Claude Code CLI in the session
      await this.launchClaudeCodeCLI();
    } catch (error) {
      if (error instanceof CircuitOpenError) {
        logger.error("Failed to launch session - circuit breaker open", error as Error);
        throw new Error("Daemon is unresponsive. Cannot launch session.");
      }

      logger.error("Failed to launch session", error as Error, { workspacePath });
      this.sessionState = "error";
      throw error;
    }
  }

  /**
   * Launch Claude Code CLI in the session
   * Sends the claude command to start the interactive session
   */
  private async launchClaudeCodeCLI(): Promise<void> {
    logger.info("Launching Claude Code CLI");

    const circuitBreaker = getDaemonCircuitBreaker();

    try {
      // Send the claude command to start Claude Code
      await circuitBreaker.execute(async () => {
        await invoke("send_terminal_input", {
          id: SessionManager.SESSION_ID,
          data: "claude\n",
        });
      });

      this.sessionState = "working";
      logger.info("Claude Code CLI launched");
    } catch (error) {
      logger.error("Failed to launch Claude Code CLI", error as Error);
      this.sessionState = "error";
      throw error;
    }
  }

  /**
   * Destroy the Claude Code session
   *
   * Cleans up the tmux session and releases resources.
   */
  async destroySession(): Promise<void> {
    if (!this.sessionExists) {
      logger.warn("No session to destroy");
      return;
    }

    logger.info("Destroying Claude Code session");

    const circuitBreaker = getDaemonCircuitBreaker();

    try {
      await circuitBreaker.execute(async () => {
        await invoke("destroy_terminal", {
          id: SessionManager.SESSION_ID,
        });
      });

      this.sessionExists = false;
      this.sessionState = "stopped";
      this.workspacePath = null;
      this.lastByteCount = 0;

      logger.info("Claude Code session destroyed");
    } catch (error) {
      if (error instanceof CircuitOpenError) {
        logger.warn("Cannot destroy session - circuit breaker open");
      } else {
        logger.error("Failed to destroy session", error as Error);
      }
      // Still mark as stopped since we can't recover
      this.sessionState = "stopped";
    }
  }

  /**
   * Get the current session status
   */
  getSessionStatus(): SessionStatus {
    return {
      exists: this.sessionExists,
      state: this.sessionState,
      sessionName: SessionManager.TMUX_SESSION,
    };
  }

  /**
   * Send input to the Claude Code session
   *
   * @param data - The input data to send (keystrokes, commands, etc.)
   * @throws Error if session doesn't exist or send fails
   */
  async sendInput(data: string): Promise<void> {
    if (!this.sessionExists) {
      throw new Error("No active session. Call launchSession() first.");
    }

    const circuitBreaker = getDaemonCircuitBreaker();

    try {
      await circuitBreaker.execute(async () => {
        await invoke("send_terminal_input", {
          id: SessionManager.SESSION_ID,
          data,
        });
      });
    } catch (error) {
      if (error instanceof CircuitOpenError) {
        logger.warn("Cannot send input - circuit breaker open");
        throw new Error("Daemon is unresponsive. Input not sent.");
      }
      throw error;
    }
  }

  /**
   * Get output from the Claude Code session
   *
   * Returns the output since the last call (incremental) or all output
   * if lines parameter is provided.
   *
   * @param lines - Optional number of lines to return (returns all available if not specified)
   * @returns The session output as a string
   */
  async getOutput(lines?: number): Promise<string> {
    if (!this.sessionExists) {
      return "";
    }

    const circuitBreaker = getDaemonCircuitBreaker();

    try {
      const result = await circuitBreaker.execute(async () => {
        // Get incremental output since last poll
        const startByte = this.lastByteCount > 0 ? this.lastByteCount : null;

        return invoke<SessionOutput>("get_terminal_output", {
          id: SessionManager.SESSION_ID,
          startByte,
        });
      });

      // Decode base64 output
      if (result.output && result.output.length > 0) {
        const decodedBytes = this.base64ToBytes(result.output);
        const text = new TextDecoder("utf-8").decode(decodedBytes);

        // Update byte count for next incremental fetch
        this.lastByteCount = result.byte_count;

        // If lines parameter provided, return only the last N lines
        if (lines !== undefined) {
          const allLines = text.split("\n");
          return allLines.slice(-lines).join("\n");
        }

        return text;
      }

      return "";
    } catch (error) {
      if (error instanceof CircuitOpenError) {
        logger.warn("Cannot get output - circuit breaker open");
        return "";
      }
      throw error;
    }
  }

  /**
   * Decode base64 string to Uint8Array
   */
  private base64ToBytes(base64: string): Uint8Array {
    const binaryString = atob(base64);
    const bytes = new Uint8Array(binaryString.length);
    for (let i = 0; i < binaryString.length; i++) {
      bytes[i] = binaryString.charCodeAt(i);
    }
    return bytes;
  }

  /**
   * Get the session ID constant
   *
   * Returns the fixed session ID used by the SessionManager.
   * This is useful for interoperating with other parts of the system
   * that use terminal IDs.
   */
  getSessionId(): string {
    return SessionManager.SESSION_ID;
  }

  /**
   * Get the Claude Code conversation directory path
   *
   * Returns the path to Claude Code's conversation/project directory.
   * This is typically `.claude/` in the workspace root.
   *
   * Useful for Phase 5 analytics - reading tool use logs, conversation
   * history, etc.
   *
   * @returns The path to the .claude directory, or null if no workspace
   */
  getConversationDir(): string | null {
    if (!this.workspacePath) {
      return null;
    }

    // Claude Code stores project-level data in .claude/
    return `${this.workspacePath}/.claude`;
  }

  /**
   * Get the workspace path for the current session
   */
  getWorkspacePath(): string | null {
    return this.workspacePath;
  }

  /**
   * Check if the session is currently active (exists and not stopped)
   */
  isActive(): boolean {
    return this.sessionExists && this.sessionState !== "stopped";
  }

  /**
   * Update the session state
   *
   * This is used by external components (like health monitors) to
   * update the session state based on observed behavior.
   */
  setState(state: "idle" | "working" | "error" | "stopped"): void {
    this.sessionState = state;
    if (state === "stopped") {
      this.sessionExists = false;
    }
  }

  /**
   * Reset the output byte counter
   *
   * This forces the next getOutput() call to return all available
   * output instead of just incremental updates.
   */
  resetOutputCounter(): void {
    this.lastByteCount = 0;
  }
}

// Singleton instance
let sessionManagerInstance: SessionManager | null = null;

/**
 * Get the singleton SessionManager instance
 */
export function getSessionManager(): SessionManager {
  if (!sessionManagerInstance) {
    sessionManagerInstance = new SessionManager();
  }
  return sessionManagerInstance;
}

/**
 * Reset the singleton instance (for testing)
 */
export function resetSessionManager(): void {
  sessionManagerInstance = null;
}
