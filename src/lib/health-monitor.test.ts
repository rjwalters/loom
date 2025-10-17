import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getHealthMonitor, HealthMonitor } from "./health-monitor";
import type { Terminal } from "./state";
import { TerminalStatus } from "./state";

// Mock Tauri API
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock state module
vi.mock("./state", async () => {
  const actual = await vi.importActual<typeof import("./state")>("./state");
  return {
    ...actual,
    getAppState: vi.fn(),
  };
});

// Mock output-poller module
vi.mock("./output-poller", () => ({
  getOutputPoller: vi.fn(),
}));

import { invoke } from "@tauri-apps/api/tauri";
import { getOutputPoller } from "./output-poller";
import { getAppState } from "./state";

describe("HealthMonitor", () => {
  let monitor: HealthMonitor;
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;
  let mockTerminals: Terminal[];
  let mockState: any;
  let mockPoller: any;

  // Factory function to create fresh mock terminals
  function createMockTerminals(): Terminal[] {
    return [
      {
        id: "terminal-1",
        name: "Test Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: true,
      },
      {
        id: "terminal-2",
        name: "Test Terminal 2",
        status: TerminalStatus.Busy,
        isPrimary: false,
      },
    ];
  }

  beforeEach(() => {
    // Reset mocks
    vi.clearAllMocks();
    vi.useFakeTimers();

    // Create fresh mock data for each test
    mockTerminals = createMockTerminals();

    mockState = {
      getTerminals: vi.fn(() => mockTerminals),
      updateTerminal: vi.fn(),
    };

    mockPoller = {
      getErrorState: vi.fn(() => ({ consecutiveErrors: 0 })),
    };

    // Setup console spies
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Setup mock implementations
    vi.mocked(getAppState).mockReturnValue(mockState as any);
    vi.mocked(getOutputPoller).mockReturnValue(mockPoller as any);
    vi.mocked(invoke).mockResolvedValue({ has_session: true });

    // Create fresh monitor instance
    monitor = new HealthMonitor();
  });

  afterEach(() => {
    monitor.stop();
    consoleLogSpy.mockRestore();
    consoleWarnSpy.mockRestore();
    consoleErrorSpy.mockRestore();
    vi.useRealTimers();
  });

  describe("Start and Stop", () => {
    it("starts monitoring with initial checks", async () => {
      monitor.start();

      expect(consoleLogSpy).toHaveBeenCalledWith("[HealthMonitor] Starting health monitoring");

      // Should perform initial health check and ping
      await vi.runOnlyPendingTimersAsync();

      expect(invoke).toHaveBeenCalledWith("check_session_health", {
        id: "terminal-1",
      });
      expect(invoke).toHaveBeenCalledWith("check_daemon_health");
    });

    it("prevents starting multiple times", () => {
      monitor.start();
      monitor.start();

      expect(consoleWarnSpy).toHaveBeenCalledWith("[HealthMonitor] Already running");
    });

    it("stops monitoring and clears timers", () => {
      monitor.start();
      monitor.stop();

      expect(consoleLogSpy).toHaveBeenCalledWith("[HealthMonitor] Stopping health monitoring");

      // Verify no more periodic checks after stop
      const invokeCallsBefore = vi.mocked(invoke).mock.calls.length;
      vi.advanceTimersByTime(60000); // Advance 60 seconds
      const invokeCallsAfter = vi.mocked(invoke).mock.calls.length;

      // Should be no new calls (only the initial checks from start)
      expect(invokeCallsAfter).toBe(invokeCallsBefore);
    });

    it("handles stop when not running", () => {
      monitor.stop();
      // Should not throw or log warning
      expect(consoleWarnSpy).not.toHaveBeenCalled();
    });
  });

  describe("Activity Recording", () => {
    it("records activity with current timestamp", () => {
      const now = Date.now();
      vi.setSystemTime(now);

      monitor.recordActivity("terminal-1");

      expect(consoleLogSpy).toHaveBeenCalledWith(
        `[HealthMonitor] Activity recorded for terminal-1 at ${now}`
      );

      const lastActivity = monitor.getLastActivity("terminal-1");
      expect(lastActivity).toBe(now);
    });

    it("updates activity timestamp on subsequent recordings", () => {
      const time1 = 1000000;
      const time2 = 2000000;

      vi.setSystemTime(time1);
      monitor.recordActivity("terminal-1");

      vi.setSystemTime(time2);
      monitor.recordActivity("terminal-1");

      const lastActivity = monitor.getLastActivity("terminal-1");
      expect(lastActivity).toBe(time2);
    });

    it("returns null for terminal with no recorded activity", () => {
      const lastActivity = monitor.getLastActivity("unknown-terminal");
      expect(lastActivity).toBeNull();
    });

    it("tracks activity for multiple terminals independently", () => {
      const time1 = 1000000;
      const time2 = 2000000;

      vi.setSystemTime(time1);
      monitor.recordActivity("terminal-1");

      vi.setSystemTime(time2);
      monitor.recordActivity("terminal-2");

      expect(monitor.getLastActivity("terminal-1")).toBe(time1);
      expect(monitor.getLastActivity("terminal-2")).toBe(time2);
    });
  });

  describe("Health Check Operations", () => {
    it("performs health check for non-busy terminals", async () => {
      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      // Should check terminal-1 (Idle status)
      expect(invoke).toHaveBeenCalledWith("check_session_health", {
        id: "terminal-1",
      });

      // Should NOT check terminal-2 (Busy status - skipped)
      const terminal2Check = vi
        .mocked(invoke)
        .mock.calls.find(
          (call) => call[0] === "check_session_health" && (call[1] as any)?.id === "terminal-2"
        );
      expect(terminal2Check).toBeUndefined();
    });

    it("skips health check for busy terminals", async () => {
      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const busyTerminalCheck = vi
        .mocked(invoke)
        .mock.calls.find(
          (call) => call[0] === "check_session_health" && (call[1] as any)?.id === "terminal-2"
        );

      // terminal-2 is busy, should be skipped
      expect(busyTerminalCheck).toBeUndefined();
    });

    it("detects missing session and updates terminal status", async () => {
      vi.mocked(invoke).mockResolvedValueOnce({ has_session: false });

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Error,
        missingSession: true,
      });
    });

    it("detects recovered session and clears error state", async () => {
      // Setup terminal with missing session
      mockTerminals[0].missingSession = true;
      mockTerminals[0].status = TerminalStatus.Error;

      vi.mocked(invoke).mockResolvedValueOnce({ has_session: true });

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
    });

    it("handles health check errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValueOnce(new Error("IPC timeout"));

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(consoleErrorSpy).toHaveBeenCalledWith(
        expect.stringContaining("[HealthMonitor] Health check failed for terminal-1"),
        expect.any(Error)
      );

      // Should not set missingSession on IPC failure
      expect(mockState.updateTerminal).not.toHaveBeenCalled();
    });

    it("detects stale terminals based on activity threshold", async () => {
      const now = Date.now();
      vi.setSystemTime(now);

      // Record activity 20 minutes ago (default stale threshold is 15 minutes)
      const activityTime = now - 20 * 60 * 1000;
      monitor.recordActivity("terminal-1");
      vi.setSystemTime(activityTime);
      monitor.recordActivity("terminal-1");
      vi.setSystemTime(now);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      const terminal1Health = health.terminals.get("terminal-1");

      expect(terminal1Health?.isStale).toBe(true);
      expect(terminal1Health?.timeSinceActivity).toBeGreaterThan(15 * 60 * 1000);
    });

    it("runs periodic health checks at configured interval", async () => {
      monitor.setHealthCheckInterval(5000); // 5 seconds
      monitor.start();

      // Initial check
      await vi.runOnlyPendingTimersAsync();
      const initialCalls = vi.mocked(invoke).mock.calls.length;

      // Advance 5 seconds
      vi.advanceTimersByTime(5000);
      await vi.runOnlyPendingTimersAsync();

      // Should have new health check calls
      expect(vi.mocked(invoke).mock.calls.length).toBeGreaterThan(initialCalls);
    });
  });

  describe("Daemon Health Monitoring", () => {
    it("pings daemon successfully", async () => {
      vi.mocked(invoke).mockResolvedValueOnce(true);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(invoke).toHaveBeenCalledWith("check_daemon_health");

      const health = monitor.getHealth();
      expect(health.daemon.connected).toBe(true);
      expect(health.daemon.consecutiveFailures).toBe(0);
    });

    it("tracks consecutive daemon ping failures", async () => {
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "check_daemon_health") {
          return Promise.reject(new Error("Daemon unreachable"));
        }
        return Promise.resolve({ has_session: true });
      });

      monitor.start();

      // First failure
      await vi.runOnlyPendingTimersAsync();
      let health = monitor.getHealth();
      expect(health.daemon.consecutiveFailures).toBe(1);
      expect(health.daemon.connected).toBe(true); // Still connected (< 3 failures)

      // Second failure
      vi.advanceTimersByTime(10000);
      await vi.runOnlyPendingTimersAsync();
      health = monitor.getHealth();
      expect(health.daemon.consecutiveFailures).toBe(2);
      expect(health.daemon.connected).toBe(true);

      // Third failure - marks as disconnected
      vi.advanceTimersByTime(10000);
      await vi.runOnlyPendingTimersAsync();
      health = monitor.getHealth();
      expect(health.daemon.consecutiveFailures).toBe(3);
      expect(health.daemon.connected).toBe(false);
    });

    it("resets failure count on successful ping", async () => {
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "check_daemon_health") {
          // Fail first, succeed second
          if (
            vi.mocked(invoke).mock.calls.filter((c) => c[0] === "check_daemon_health").length <= 1
          ) {
            return Promise.reject(new Error("Daemon unreachable"));
          }
          return Promise.resolve(true);
        }
        return Promise.resolve({ has_session: true });
      });

      monitor.start();

      // First ping fails
      await vi.runOnlyPendingTimersAsync();
      let health = monitor.getHealth();
      expect(health.daemon.consecutiveFailures).toBe(1);

      // Second ping succeeds
      vi.advanceTimersByTime(10000);
      await vi.runOnlyPendingTimersAsync();
      health = monitor.getHealth();
      expect(health.daemon.consecutiveFailures).toBe(0);
      expect(health.daemon.connected).toBe(true);
    });

    it("runs periodic daemon pings at configured interval", async () => {
      monitor.setDaemonPingInterval(3000); // 3 seconds
      monitor.start();

      // Initial ping
      await vi.runOnlyPendingTimersAsync();
      const initialPingCalls = vi
        .mocked(invoke)
        .mock.calls.filter((c) => c[0] === "check_daemon_health").length;

      // Advance 3 seconds
      vi.advanceTimersByTime(3000);
      await vi.runOnlyPendingTimersAsync();

      const newPingCalls = vi
        .mocked(invoke)
        .mock.calls.filter((c) => c[0] === "check_daemon_health").length;

      expect(newPingCalls).toBeGreaterThan(initialPingCalls);
    });
  });

  describe("Health Snapshot", () => {
    it("returns current health snapshot with all metrics", async () => {
      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();

      expect(health).toMatchObject({
        activeTerminals: expect.any(Number),
        healthyTerminals: expect.any(Number),
        errorTerminals: expect.any(Number),
        lastCheckTime: expect.any(Number),
      });

      expect(health.terminals).toBeInstanceOf(Map);
      expect(health.daemon).toMatchObject({
        connected: expect.any(Boolean),
        lastPing: expect.any(Number),
        timeSincePing: expect.any(Number),
        consecutiveFailures: expect.any(Number),
      });
    });

    it("counts active terminals correctly", async () => {
      mockTerminals[0].status = TerminalStatus.Idle;
      mockTerminals[1].status = TerminalStatus.Busy;

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      // Both terminals are active (not stopped)
      expect(health.activeTerminals).toBe(2);
    });

    it("counts error terminals correctly", async () => {
      vi.mocked(invoke).mockResolvedValueOnce({ has_session: false });

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      expect(health.errorTerminals).toBe(1); // terminal-1 has missing session
    });

    it("includes poller error state in health", async () => {
      mockPoller.getErrorState.mockReturnValueOnce({ consecutiveErrors: 3 });

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      const terminal1Health = health.terminals.get("terminal-1");

      expect(terminal1Health?.pollerErrors).toBe(3);
    });
  });

  describe("Health Update Callbacks", () => {
    it("notifies callbacks on health check completion", async () => {
      const callback = vi.fn();
      monitor.onHealthUpdate(callback);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(callback).toHaveBeenCalled();
      expect(callback.mock.calls[0][0]).toMatchObject({
        activeTerminals: expect.any(Number),
        healthyTerminals: expect.any(Number),
        errorTerminals: expect.any(Number),
      });
    });

    it("notifies callbacks on daemon ping completion", async () => {
      const callback = vi.fn();
      monitor.onHealthUpdate(callback);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      // Should be called for both health check and daemon ping
      expect(callback.mock.calls.length).toBeGreaterThanOrEqual(1);
    });

    it("supports multiple independent callbacks", async () => {
      const callback1 = vi.fn();
      const callback2 = vi.fn();

      monitor.onHealthUpdate(callback1);
      monitor.onHealthUpdate(callback2);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(callback1).toHaveBeenCalled();
      expect(callback2).toHaveBeenCalled();
    });

    it("returns unsubscribe function", async () => {
      const callback = vi.fn();
      const unsubscribe = monitor.onHealthUpdate(callback);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(callback).toHaveBeenCalled();

      callback.mockClear();
      unsubscribe();

      // Trigger another health check
      await monitor.performHealthCheck();

      expect(callback).not.toHaveBeenCalled();
    });

    it("handles errors in callbacks gracefully", async () => {
      const badCallback = vi.fn(() => {
        throw new Error("Callback error");
      });
      const goodCallback = vi.fn();

      monitor.onHealthUpdate(badCallback);
      monitor.onHealthUpdate(goodCallback);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      expect(consoleErrorSpy).toHaveBeenCalledWith(
        "[HealthMonitor] Error in health callback:",
        expect.any(Error)
      );

      // Good callback should still be called
      expect(goodCallback).toHaveBeenCalled();
    });
  });

  describe("Configuration", () => {
    it("allows configuring stale threshold", async () => {
      monitor.setStaleThreshold(5 * 60 * 1000); // 5 minutes

      const now = Date.now();
      vi.setSystemTime(now);

      // Record activity 6 minutes ago
      const activityTime = now - 6 * 60 * 1000;
      vi.setSystemTime(activityTime);
      monitor.recordActivity("terminal-1");
      vi.setSystemTime(now);

      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      const terminal1Health = health.terminals.get("terminal-1");

      expect(terminal1Health?.isStale).toBe(true);
    });

    it("allows configuring health check interval", async () => {
      monitor.setHealthCheckInterval(2000); // 2 seconds
      monitor.start();

      await vi.runOnlyPendingTimersAsync();
      const callsBeforeInterval = vi.mocked(invoke).mock.calls.length;

      vi.advanceTimersByTime(2000);
      await vi.runOnlyPendingTimersAsync();

      const callsAfterInterval = vi.mocked(invoke).mock.calls.length;
      expect(callsAfterInterval).toBeGreaterThan(callsBeforeInterval);
    });

    it("allows configuring daemon ping interval", async () => {
      monitor.setDaemonPingInterval(1000); // 1 second
      monitor.start();

      await vi.runOnlyPendingTimersAsync();
      const initialPings = vi
        .mocked(invoke)
        .mock.calls.filter((c) => c[0] === "check_daemon_health").length;

      vi.advanceTimersByTime(1000);
      await vi.runOnlyPendingTimersAsync();

      const newPings = vi
        .mocked(invoke)
        .mock.calls.filter((c) => c[0] === "check_daemon_health").length;

      expect(newPings).toBeGreaterThan(initialPings);
    });

    it("restarts timer when changing interval while running", async () => {
      monitor.setHealthCheckInterval(10000); // 10 seconds
      monitor.start();

      await vi.runOnlyPendingTimersAsync();

      // Change interval while running
      monitor.setHealthCheckInterval(1000); // 1 second

      vi.advanceTimersByTime(1000);
      await vi.runOnlyPendingTimersAsync();

      // Should have triggered with new interval
      expect(vi.mocked(invoke).mock.calls.length).toBeGreaterThan(2);
    });
  });

  describe("Singleton Instance", () => {
    it("returns same instance from getHealthMonitor", () => {
      const instance1 = getHealthMonitor();
      const instance2 = getHealthMonitor();

      expect(instance1).toBe(instance2);
    });
  });

  describe("Real-world Scenarios", () => {
    it("monitors healthy terminal lifecycle", async () => {
      const callback = vi.fn();
      monitor.onHealthUpdate(callback);

      // Start monitoring
      monitor.start();
      await vi.runOnlyPendingTimersAsync();

      let health = monitor.getHealth();
      expect(health.healthyTerminals).toBe(1); // terminal-1 is healthy

      // Record activity
      monitor.recordActivity("terminal-1");

      // Wait for next health check
      vi.advanceTimersByTime(30000);
      await vi.runOnlyPendingTimersAsync();

      health = monitor.getHealth();
      const terminal1Health = health.terminals.get("terminal-1");
      expect(terminal1Health?.hasSession).toBe(true);
      expect(terminal1Health?.isStale).toBe(false);
    });

    it("detects terminal session loss and recovery", async () => {
      monitor.start();

      // Initial state - session exists
      vi.mocked(invoke).mockResolvedValueOnce({ has_session: true });
      await vi.runOnlyPendingTimersAsync();

      const health = monitor.getHealth();
      expect(health.terminals.get("terminal-1")?.hasSession).toBe(true);

      // Session lost
      vi.mocked(invoke).mockResolvedValueOnce({ has_session: false });
      vi.advanceTimersByTime(30000);
      await vi.runOnlyPendingTimersAsync();

      expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Error,
        missingSession: true,
      });

      // Session recovered
      mockTerminals[0].missingSession = true;
      vi.mocked(invoke).mockResolvedValueOnce({ has_session: true });
      vi.advanceTimersByTime(30000);
      await vi.runOnlyPendingTimersAsync();

      expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
    });

    it("handles daemon connectivity loss gracefully", async () => {
      monitor.start();

      // Initial successful ping
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "check_daemon_health") {
          return Promise.resolve(true);
        }
        return Promise.resolve({ has_session: true });
      });

      await vi.runOnlyPendingTimersAsync();
      let health = monitor.getHealth();
      expect(health.daemon.connected).toBe(true);

      // Daemon becomes unreachable
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "check_daemon_health") {
          return Promise.reject(new Error("Connection refused"));
        }
        return Promise.resolve({ has_session: true });
      });

      // 3 consecutive failures
      for (let i = 0; i < 3; i++) {
        vi.advanceTimersByTime(10000);
        await vi.runOnlyPendingTimersAsync();
      }

      health = monitor.getHealth();
      expect(health.daemon.connected).toBe(false);
      expect(health.daemon.consecutiveFailures).toBe(3);
    });
  });
});
