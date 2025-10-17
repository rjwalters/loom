import { invoke } from "@tauri-apps/api/tauri";
import { getAppState, TerminalStatus } from "./state";

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
      console.warn("[HealthMonitor] Already running");
      return;
    }

    console.log("[HealthMonitor] Starting health monitoring");
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

    console.log("[HealthMonitor] Stopping health monitoring");
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
   */
  recordActivity(terminalId: string): void {
    const now = Date.now();
    this.terminalActivity.set(terminalId, now);
    console.log(`[HealthMonitor] Activity recorded for ${terminalId} at ${now}`);
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
    const terminals = state.getTerminals();

    let activeTerminals = 0;
    let healthyTerminals = 0;
    let errorTerminals = 0;

    for (const terminal of terminals) {
      const health = this.terminalHealth.get(terminal.id);
      if (!health) continue;

      if (terminal.status !== TerminalStatus.Stopped) {
        activeTerminals++;
      }

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
    const terminals = state.getTerminals();
    const now = Date.now();

    console.log(`[HealthMonitor] Performing health check for ${terminals.length} terminals`);

    for (const terminal of terminals) {
      try {
        // Check if tmux session exists
        const result = await invoke<{ has_session: boolean }>("check_session_health", {
          id: terminal.id,
        });
        const hasSession = result.has_session;

        // Get last activity time
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
          console.warn(`[HealthMonitor] Terminal ${terminal.id} missing tmux session`);
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Error,
            missingSession: true,
          });
        } else if (hasSession && terminal.missingSession) {
          // Session recovered - clear error state
          console.log(
            `[HealthMonitor] Terminal ${terminal.id} session recovered, clearing missingSession flag`
          );
          state.updateTerminal(terminal.id, {
            status: TerminalStatus.Idle,
            missingSession: undefined,
          });
        }
      } catch (error) {
        // Health check failed (daemon unreachable, IPC timeout, tmux server down, etc.)
        // DO NOT set missingSession=true - we simply couldn't check
        // This prevents false positives when daemon/tmux is temporarily unavailable
        console.error(
          `[HealthMonitor] Health check failed for ${terminal.id} (not changing missingSession state):`,
          error
        );

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

      console.log(`[HealthMonitor] Daemon ping successful at ${now}`);
    } catch (error) {
      this.daemonHealth.consecutiveFailures++;
      this.daemonHealth.connected = this.daemonHealth.consecutiveFailures < 3; // Mark disconnected after 3 failures

      const timeSincePing = this.daemonHealth.lastPing
        ? Date.now() - this.daemonHealth.lastPing
        : null;
      this.daemonHealth.timeSincePing = timeSincePing;

      console.error(
        `[HealthMonitor] Daemon ping failed (${this.daemonHealth.consecutiveFailures} consecutive):`,
        error
      );
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
        console.error("[HealthMonitor] Error in health callback:", error);
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
