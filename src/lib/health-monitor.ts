/**
 * health-monitor.ts - Coordinates periodic health monitoring
 *
 * Architecture:
 * - This class is the SCHEDULER: decides WHEN to perform health checks
 * - For session health, uses IPC `check_session_health` command to verify tmux sessions exist
 * - For daemon health, uses IPC `check_daemon_health` command (ping every 10s)
 * - For activity tracking, receives callbacks from output-poller (recordActivity method)
 *
 * Separation of Concerns:
 * - health-monitor.ts (this file): SCHEDULER - periodic checks, activity timestamps, daemon connectivity
 * - terminal-probe.ts: CHECKER - terminal TYPE detection (agent vs shell) via probe commands
 *
 * These modules are intentionally separate:
 * - Health monitoring checks if terminals are ALIVE (session exists, responding)
 * - Terminal probing checks what TYPE a terminal is (AI agent vs plain shell)
 *
 * DO NOT add terminal command sending logic here - health checks use IPC only.
 * For terminal type detection, see terminal-probe.ts instead.
 *
 * @see terminal-probe.ts for terminal type detection
 * @see output-poller.ts for activity tracking integration
 */

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import { getAppState, TerminalStatus } from "./state";

const logger = Logger.forComponent("health-monitor");

/**
 * Health check result for a terminal
 */
export interface TerminalHealth {
  terminalId: string;
  hasSession: boolean; // Tmux session exists
  lastActivity: number | null; // Timestamp of last output (ms)
  timeSinceActivity: number | null; // Milliseconds since last activity
  isStale: boolean; // True if no activity for > staleThreshold
  pollerErrors: number; // Consecutive poller errors
}

/**
 * Daemon connection health
 */
export interface DaemonHealth {
  connected: boolean; // Can reach daemon via IPC
  lastPing: number | null; // Last successful ping timestamp
  timeSincePing: number | null; // Milliseconds since last ping
  consecutiveFailures: number; // Consecutive ping failures
}

/**
 * Overall system health snapshot
 */
export interface SystemHealth {
  terminals: Map<string, TerminalHealth>;
  daemon: DaemonHealth;
  activeTerminals: number;
  healthyTerminals: number;
  errorTerminals: number;
  lastCheckTime: number;
}

/**
 * HealthMonitor - Monitors terminal and daemon health
 *
 * Features:
 * - Periodic health checks (every 30s by default)
 * - Activity tracking (last output time per terminal)
 * - Daemon connectivity monitoring (ping every 10s)
 * - Stale terminal detection (no output for configurable threshold)
 * - Health status callbacks for UI updates
 */
export class HealthMonitor {
  private healthCheckInterval: number = 30000; // 30 seconds
  private daemonPingInterval: number = 10000; // 10 seconds
  private staleThreshold: number = 15 * 60 * 1000; // 15 minutes

  private healthCheckTimer: number | null = null;
  private daemonPingTimer: number | null = null;

  private terminalActivity: Map<string, number> = new Map(); // terminalId -> last activity timestamp
  private terminalHealth: Map<string, TerminalHealth> = new Map();

  private daemonHealth: DaemonHealth = {
    connected: false,
    lastPing: null,
    timeSincePing: null,
    consecutiveFailures: 0,
  };

  private healthCallbacks: Set<(health: SystemHealth) => void> = new Set();
  private running: boolean = false;

  /**
   * Start health monitoring
   */
  start(): void {
    if (this.running) {
      logger.warn("Already running");
      return;
    }

    logger.info("Starting health monitoring");
    this.running = true;

    // Initial checks
    this.performHealthCheck();
    this.pingDaemon();

    // Start periodic health checks
    this.healthCheckTimer = window.setInterval(() => {
      this.performHealthCheck();
    }, this.healthCheckInterval);

    // Start periodic daemon pings
    this.daemonPingTimer = window.setInterval(() => {
      this.pingDaemon();
    }, this.daemonPingInterval);
  }

  /**
   * Stop health monitoring
   */
  stop(): void {
    if (!this.running) {
      return;
    }

    logger.info("Stopping health monitoring");
    this.running = false;

    if (this.healthCheckTimer !== null) {
      window.clearInterval(this.healthCheckTimer);
      this.healthCheckTimer = null;
    }

    if (this.daemonPingTimer !== null) {
      window.clearInterval(this.daemonPingTimer);
      this.daemonPingTimer = null;
    }
  }

  /**
   * Record activity for a terminal (called when output is received)
   *
   * Integration point: This method is called by output-poller when a terminal
   * produces output. It updates the activity timestamp used to detect stale
   * terminals (no output for > staleThreshold).
   *
   * This is a passive observer pattern - we don't send commands to detect activity,
   * we just record when output is observed. For active probing (sending commands
   * to detect terminal type), see terminal-probe.ts instead.
   */
  recordActivity(terminalId: string): void {
    const now = Date.now();
    this.terminalActivity.set(terminalId, now);
    logger.info("Activity recorded", { terminalId, timestamp: now });
  }

  /**
   * Get last activity time for a terminal
   */
  getLastActivity(terminalId: string): number | null {
    return this.terminalActivity.get(terminalId) || null;
  }

  /**
   * Get current health snapshot
   */
  getHealth(): SystemHealth {
    const state = getAppState();
    const terminals = state.terminals.getTerminals();

    let activeTerminals = 0;
    let healthyTerminals = 0;
    let errorTerminals = 0;

    for (const terminal of terminals) {
      // Count active terminals based on status alone (don't require health record)
      if (terminal.status !== TerminalStatus.Stopped) {
        activeTerminals++;
      }

      const health = this.terminalHealth.get(terminal.id);
      if (!health) continue;

      if (health.hasSession && !health.isStale && health.pollerErrors === 0) {
        healthyTerminals++;
      }

      if (terminal.status === TerminalStatus.Error || !health.hasSession) {
        errorTerminals++;
      }
    }

    return {
      terminals: new Map(this.terminalHealth),
      daemon: { ...this.daemonHealth },
      activeTerminals,
      healthyTerminals,
      errorTerminals,
      lastCheckTime: Date.now(),
    };
  }

  /**
   * Get health check timing information for UI display
   */
  getHealthCheckTiming(): {
    lastCheckTime: number | null;
    nextCheckTime: number | null;
    checkIntervalMs: number;
  } {
    // Get last check time from the most recent terminal health record
    let lastCheckTime: number | null = null;
    if (this.terminalHealth.size > 0 && this.running) {
      // We don't store individual check times, so we'll use a conservative estimate
      // based on the health monitor being active
      lastCheckTime = Date.now(); // Approximate - health checks happen periodically
    }

    // Calculate next check time based on interval
    const nextCheckTime =
      this.running && lastCheckTime ? lastCheckTime + this.healthCheckInterval : null;

    return {
      lastCheckTime,
      nextCheckTime,
      checkIntervalMs: this.healthCheckInterval,
    };
  }

  /**
   * Subscribe to health updates
   */
  onHealthUpdate(callback: (health: SystemHealth) => void): () => void {
    this.healthCallbacks.add(callback);
    return () => this.healthCallbacks.delete(callback);
  }

  /**
   * Configure stale threshold (milliseconds of inactivity)
   */
  setStaleThreshold(ms: number): void {
    this.staleThreshold = ms;
  }

  /**
   * Configure health check interval
   */
  setHealthCheckInterval(ms: number): void {
    this.healthCheckInterval = ms;
    // Restart timer if running
    if (this.running && this.healthCheckTimer !== null) {
      window.clearInterval(this.healthCheckTimer);
      this.healthCheckTimer = window.setInterval(() => {
        this.performHealthCheck();
      }, this.healthCheckInterval);
    }
  }

  /**
   * Configure daemon ping interval
   */
  setDaemonPingInterval(ms: number): void {
    this.daemonPingInterval = ms;
    // Restart timer if running
    if (this.running && this.daemonPingTimer !== null) {
      window.clearInterval(this.daemonPingTimer);
      this.daemonPingTimer = window.setInterval(() => {
        this.pingDaemon();
      }, this.daemonPingInterval);
    }
  }

  /**
   * Perform health check on all terminals
   * Public to allow immediate health checks on workspace start
   */
  public async performHealthCheck(): Promise<void> {
    const state = getAppState();
    const terminals = state.terminals.getTerminals();
    const now = Date.now();

    logger.info("Performing health check", { terminalCount: terminals.length });

    for (const terminal of terminals) {
      // Debug: Log exact terminal state before skip check
      logger.info("Checking terminal", {
        terminalId: terminal.id,
        name: terminal.name,
        status: terminal.status,
        missingSession: terminal.missingSession || false,
      });

      // Skip health checks for terminals that are busy (agent launching in progress)
      // This prevents false positives during agent launch which can take 20+ seconds per terminal
      if (terminal.status === TerminalStatus.Busy) {
        logger.info("Skipping health check - agent launch in progress", {
          terminalId: terminal.id,
          status: terminal.status,
        });
        continue;
      }

      try {
        // Check if tmux session exists via IPC (daemon verifies session)
        // NOTE: This checks if terminal is ALIVE, not what TYPE it is.
        // For terminal type detection (agent vs shell), see terminal-probe.ts
        logger.info("Checking session health", { terminalId: terminal.id });
        const hasSession = await invoke<boolean>("check_session_health", {
          id: terminal.id,
        });
        logger.info("Session health check result", {
          terminalId: terminal.id,
          hasSession,
        });

        // Get last activity time (populated by recordActivity callback from output-poller)
        // Integration point: output-poller calls recordActivity() when terminal produces output
        const lastActivity = this.terminalActivity.get(terminal.id) || null;
        const timeSinceActivity = lastActivity ? now - lastActivity : null;
        const isStale = timeSinceActivity !== null && timeSinceActivity > this.staleThreshold;

        // Get poller error state (if available)
        const { getOutputPoller } = await import("./output-poller");
        const poller = getOutputPoller();
        const errorState = poller.getErrorState(terminal.id);
        const pollerErrors = errorState?.consecutiveErrors || 0;

        const health: TerminalHealth = {
          terminalId: terminal.id,
          hasSession,
          lastActivity,
          timeSinceActivity,
          isStale,
          pollerErrors,
        };

        this.terminalHealth.set(terminal.id, health);

        // Update terminal status based on session health
        // Only update missingSession flag if health check SUCCEEDED
        if (!hasSession && !terminal.missingSession) {
          // Session missing - mark as error
          logger.warn("Terminal missing tmux session", { terminalId: terminal.id });
          state.terminals.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        } else if (hasSession && terminal.missingSession) {
          // Session recovered - clear error state
          logger.info("Terminal session recovered", { terminalId: terminal.id });
          state.terminals.updateTerminal(terminal.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        }
      } catch (error) {
        // Health check failed (daemon unreachable, IPC timeout, tmux server down, etc.)
        // DO NOT set missingSession=true - we simply couldn't check
        // This prevents false positives when daemon/tmux is temporarily unavailable
        logger.error("Health check failed - not changing missingSession state", error, {
          terminalId: terminal.id,
        });

        // Still update health record to show check failed
        const lastActivity = this.terminalActivity.get(terminal.id) || null;
        const timeSinceActivity = lastActivity ? now - lastActivity : null;

        this.terminalHealth.set(terminal.id, {
          terminalId: terminal.id,
          hasSession: false, // Unknown, but mark as false in health record
          lastActivity,
          timeSinceActivity,
          isStale: false, // Can't determine if we couldn't check
          pollerErrors: 0,
        });
      }
    }

    // Notify listeners
    this.notifyHealthUpdate();
  }

  /**
   * Ping daemon to check connectivity
   */
  private async pingDaemon(): Promise<void> {
    try {
      // Send ping request (daemon should respond with true if connected)
      await invoke<boolean>("check_daemon_health");

      const now = Date.now();
      this.daemonHealth = {
        connected: true,
        lastPing: now,
        timeSincePing: 0,
        consecutiveFailures: 0,
      };

      logger.info("Daemon ping successful", { timestamp: now });
    } catch (error) {
      this.daemonHealth.consecutiveFailures++;
      this.daemonHealth.connected = this.daemonHealth.consecutiveFailures < 3; // Mark disconnected after 3 failures

      const timeSincePing = this.daemonHealth.lastPing
        ? Date.now() - this.daemonHealth.lastPing
        : null;
      this.daemonHealth.timeSincePing = timeSincePing;

      logger.error("Daemon ping failed", error, {
        consecutiveFailures: this.daemonHealth.consecutiveFailures,
        connected: this.daemonHealth.connected,
      });
    }

    // Notify listeners
    this.notifyHealthUpdate();
  }

  /**
   * Notify all health update callbacks
   */
  private notifyHealthUpdate(): void {
    const health = this.getHealth();
    this.healthCallbacks.forEach((callback) => {
      try {
        callback(health);
      } catch (error) {
        logger.error("Error in health callback", error);
      }
    });
  }
}

// Singleton instance
let healthMonitorInstance: HealthMonitor | null = null;

export function getHealthMonitor(): HealthMonitor {
  if (!healthMonitorInstance) {
    healthMonitorInstance = new HealthMonitor();
  }
  return healthMonitorInstance;
}
