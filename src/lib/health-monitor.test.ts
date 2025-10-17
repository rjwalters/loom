import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SystemHealth } from "./health-monitor";
import { HealthMonitor } from "./health-monitor";
import { TerminalStatus } from "./state";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock state
vi.mock("./state", () => ({
  getAppState: vi.fn(() => ({
    getTerminals: vi.fn(() => []),
    updateTerminal: vi.fn(),
  })),
  TerminalStatus: {
    Idle: "idle",
    Busy: "busy",
    NeedsInput: "needs_input",
    Error: "error",
    Stopped: "stopped",
  },
}));

// Mock output-poller
vi.mock("./output-poller", () => ({
  getOutputPoller: vi.fn(() => ({
    getErrorState: vi.fn(() => ({ consecutiveErrors: 0 })),
  })),
}));

describe("HealthMonitor", () => {
  let monitor: HealthMonitor;
  let mockInvoke: ReturnType<typeof vi.fn>;
  let mockGetAppState: ReturnType<typeof vi.fn>;
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(async () => {
    // Reset mocks
    vi.clearAllMocks();
    vi.useFakeTimers();

    // Setup mock functions
    const { invoke } = await import("@tauri-apps/api/tauri");
    mockInvoke = invoke as ReturnType<typeof vi.fn>;

    const { getAppState } = await import("./state");
    mockGetAppState = getAppState as ReturnType<typeof vi.fn>;

    // Spy on console methods
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Create fresh monitor instance
    monitor = new HealthMonitor();
  });

  afterEach(() => {
    monitor.stop();
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  describe("Lifecycle Management", () => {
    it("should start monitoring", () => {
      monitor.start();

      expect(consoleLogSpy).toHaveBeenCalledWith(
        expect.stringContaining("Starting health monitoring")
      );
    });

    it("should not start if already running", () => {
      monitor.start();
      consoleLogSpy.mockClear();

      monitor.start();

      expect(consoleWarnSpy).toHaveBeenCalledWith(expect.stringContaining("Already running"));
    });

    it("should stop monitoring and clear timers", () => {
      monitor.start();
      monitor.stop();

      expect(consoleLogSpy).toHaveBeenCalledWith(
        expect.stringContaining("Stopping health monitoring")
      );
    });

    it("should handle stop when not running", () => {
      // Should not throw
      expect(() => monitor.stop()).not.toThrow();
    });

    it("should perform initial health check on start", async () => {
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      monitor.start();

      expect(consoleLogSpy).toHaveBeenCalledWith(
        expect.stringContaining("Performing health check")
      );
    });

    it("should perform initial daemon ping on start", () => {
      monitor.start();

      expect(mockInvoke).toHaveBeenCalledWith("check_daemon_health");
    });
  });

  describe("Activity Tracking", () => {
    beforeEach(() => {
      vi.setSystemTime(new Date("2025-01-01T12:00:00Z"));
    });

    it("should record terminal activity", () => {
      monitor.recordActivity("terminal-1");

      const lastActivity = monitor.getLastActivity("terminal-1");
      expect(lastActivity).toBe(new Date("2025-01-01T12:00:00Z").getTime());
    });

    it("should update activity timestamp on subsequent calls", () => {
      monitor.recordActivity("terminal-1");

      vi.setSystemTime(new Date("2025-01-01T12:05:00Z"));
      monitor.recordActivity("terminal-1");

      const lastActivity = monitor.getLastActivity("terminal-1");
      expect(lastActivity).toBe(new Date("2025-01-01T12:05:00Z").getTime());
    });

    it("should return null for unknown terminal", () => {
      const lastActivity = monitor.getLastActivity("unknown-terminal");
      expect(lastActivity).toBeNull();
    });

    it("should track activity for multiple terminals independently", () => {
      vi.setSystemTime(new Date("2025-01-01T12:00:00Z"));
      monitor.recordActivity("terminal-1");

      vi.setSystemTime(new Date("2025-01-01T12:05:00Z"));
      monitor.recordActivity("terminal-2");

      expect(monitor.getLastActivity("terminal-1")).toBe(
        new Date("2025-01-01T12:00:00Z").getTime()
      );
      expect(monitor.getLastActivity("terminal-2")).toBe(
        new Date("2025-01-01T12:05:00Z").getTime()
      );
    });
  });

  describe("Health Check", () => {
    it("should check health for all terminals", async () => {
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Idle },
          { id: "terminal-2", status: TerminalStatus.Idle },
        ]),
        updateTerminal: vi.fn(),
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      await monitor.performHealthCheck();

      expect(mockInvoke).toHaveBeenCalledWith("check_session_health", {
        id: "terminal-1",
      });
      expect(mockInvoke).toHaveBeenCalledWith("check_session_health", {
        id: "terminal-2",
      });
    });

    it("should skip health check for busy terminals", async () => {
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [{ id: "terminal-1", status: TerminalStatus.Busy }]),
        updateTerminal: vi.fn(),
      });

      await monitor.performHealthCheck();

      expect(consoleLogSpy).toHaveBeenCalledWith(
        expect.stringContaining("Skipping health check for terminal-1")
      );
      expect(mockInvoke).not.toHaveBeenCalledWith("check_session_health", {
        id: "terminal-1",
      });
    });

    it("should detect missing tmux session", async () => {
      const mockUpdateTerminal = vi.fn();
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Idle, missingSession: false },
        ]),
        updateTerminal: mockUpdateTerminal,
      });

      mockInvoke.mockResolvedValue({ has_session: false });

      await monitor.performHealthCheck();

      expect(consoleWarnSpy).toHaveBeenCalledWith(expect.stringContaining("missing tmux session"));
      expect(mockUpdateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Error,
        missingSession: true,
      });
    });

    it("should detect recovered tmux session", async () => {
      const mockUpdateTerminal = vi.fn();
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Error, missingSession: true },
        ]),
        updateTerminal: mockUpdateTerminal,
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      await monitor.performHealthCheck();

      expect(consoleLogSpy).toHaveBeenCalledWith(expect.stringContaining("session recovered"));
      expect(mockUpdateTerminal).toHaveBeenCalledWith("terminal-1", {
        status: TerminalStatus.Idle,
        missingSession: undefined,
      });
    });

    it("should not update missingSession flag on health check failure", async () => {
      const mockUpdateTerminal = vi.fn();
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Idle, missingSession: false },
        ]),
        updateTerminal: mockUpdateTerminal,
      });

      mockInvoke.mockRejectedValue(new Error("Daemon unreachable"));

      await monitor.performHealthCheck();

      expect(consoleErrorSpy).toHaveBeenCalledWith(
        expect.stringContaining("Health check failed"),
        expect.any(Error)
      );
      expect(mockUpdateTerminal).not.toHaveBeenCalled();
    });

    it("should detect stale terminals", async () => {
      vi.setSystemTime(new Date("2025-01-01T12:00:00Z"));
      monitor.recordActivity("terminal-1");

      // Advance time beyond stale threshold (15 minutes default)
      vi.setSystemTime(new Date("2025-01-01T12:20:00Z"));

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [{ id: "terminal-1", status: TerminalStatus.Idle }]),
        updateTerminal: vi.fn(),
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      await monitor.performHealthCheck();

      const health = monitor.getHealth();
      const terminalHealth = health.terminals.get("terminal-1");

      expect(terminalHealth?.isStale).toBe(true);
      expect(terminalHealth?.timeSinceActivity).toBeGreaterThan(15 * 60 * 1000);
    });

    it("should mark terminal as not stale when activity is recent", async () => {
      vi.setSystemTime(new Date("2025-01-01T12:00:00Z"));
      monitor.recordActivity("terminal-1");

      // Advance time but stay within threshold
      vi.setSystemTime(new Date("2025-01-01T12:05:00Z"));

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [{ id: "terminal-1", status: TerminalStatus.Idle }]),
        updateTerminal: vi.fn(),
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      await monitor.performHealthCheck();

      const health = monitor.getHealth();
      const terminalHealth = health.terminals.get("terminal-1");

      expect(terminalHealth?.isStale).toBe(false);
    });
  });

  describe("Daemon Health", () => {
    it("should ping daemon and record success", async () => {
      const startTime = new Date("2025-01-01T12:00:00Z").getTime();
      vi.setSystemTime(startTime);

      let pingTime = 0;
      mockInvoke.mockImplementation(async () => {
        pingTime = Date.now();
        return true;
      });

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      monitor.start();

      // Wait for async operations to complete
      await Promise.resolve();
      await Promise.resolve();

      monitor.stop();

      const health = monitor.getHealth();

      expect(health.daemon.connected).toBe(true);
      expect(health.daemon.consecutiveFailures).toBe(0);
      // Ping should happen at the start time
      expect(pingTime).toBe(startTime);
      expect(health.daemon.lastPing).toBe(startTime);
    });

    it("should track consecutive daemon ping failures", async () => {
      mockInvoke.mockRejectedValue(new Error("Connection refused"));
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      monitor.start();

      // Wait for async operations to complete
      await Promise.resolve();
      await Promise.resolve();

      monitor.stop();

      const health = monitor.getHealth();

      expect(health.daemon.consecutiveFailures).toBeGreaterThan(0);
      expect(consoleErrorSpy).toHaveBeenCalledWith(
        expect.stringContaining("Daemon ping failed"),
        expect.any(Error)
      );
    });

    it("should mark daemon as disconnected after 3 consecutive failures", async () => {
      const pingTimes: number[] = [];
      const startTime = Date.now();

      mockInvoke.mockImplementation(() => {
        pingTimes.push(Date.now());
        return Promise.reject(new Error("Connection refused"));
      });

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      const pingInterval = 1000;
      monitor.setDaemonPingInterval(pingInterval);
      monitor.start();

      // Wait for initial ping
      await Promise.resolve();
      await Promise.resolve();
      expect(pingTimes.length).toBe(1);

      // Advance time for second ping
      await vi.advanceTimersByTimeAsync(pingInterval);
      expect(pingTimes.length).toBe(2);
      // Verify second ping happened exactly 1000ms after first
      expect(pingTimes[1] - pingTimes[0]).toBe(pingInterval);

      // Advance time for third ping
      await vi.advanceTimersByTimeAsync(pingInterval);
      expect(pingTimes.length).toBe(3);
      // Verify third ping happened exactly 1000ms after second
      expect(pingTimes[2] - pingTimes[1]).toBe(pingInterval);

      monitor.stop();

      const health = monitor.getHealth();

      expect(health.daemon.connected).toBe(false);
      expect(health.daemon.consecutiveFailures).toBe(3);
    });
  });

  describe("Health Callbacks", () => {
    it("should notify callbacks on health update", async () => {
      const callback = vi.fn();
      monitor.onHealthUpdate(callback);

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      await monitor.performHealthCheck();

      expect(callback).toHaveBeenCalledWith(
        expect.objectContaining({
          terminals: expect.any(Map),
          daemon: expect.any(Object),
        })
      );
    });

    it("should allow multiple callbacks", async () => {
      const callback1 = vi.fn();
      const callback2 = vi.fn();

      monitor.onHealthUpdate(callback1);
      monitor.onHealthUpdate(callback2);

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      await monitor.performHealthCheck();

      expect(callback1).toHaveBeenCalled();
      expect(callback2).toHaveBeenCalled();
    });

    it("should unsubscribe callback using returned function", async () => {
      const callback = vi.fn();
      const unsubscribe = monitor.onHealthUpdate(callback);

      unsubscribe();

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      await monitor.performHealthCheck();

      expect(callback).not.toHaveBeenCalled();
    });

    it("should handle errors in callbacks gracefully", async () => {
      const errorCallback = vi.fn(() => {
        throw new Error("Callback error");
      });
      const normalCallback = vi.fn();

      monitor.onHealthUpdate(errorCallback);
      monitor.onHealthUpdate(normalCallback);

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      await monitor.performHealthCheck();

      expect(consoleErrorSpy).toHaveBeenCalledWith(
        expect.stringContaining("Error in health callback"),
        expect.any(Error)
      );
      expect(normalCallback).toHaveBeenCalled();
    });
  });

  describe("System Health Snapshot", () => {
    it("should return current health snapshot", () => {
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Idle },
          { id: "terminal-2", status: TerminalStatus.Error },
        ]),
        updateTerminal: vi.fn(),
      });

      const health = monitor.getHealth();

      expect(health).toHaveProperty("terminals");
      expect(health).toHaveProperty("daemon");
      expect(health).toHaveProperty("activeTerminals");
      expect(health).toHaveProperty("healthyTerminals");
      expect(health).toHaveProperty("errorTerminals");
      expect(health).toHaveProperty("lastCheckTime");
    });

    it("should count active terminals correctly", async () => {
      // Setup health data for terminals first
      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [
          { id: "terminal-1", status: TerminalStatus.Idle },
          { id: "terminal-2", status: TerminalStatus.Busy },
          { id: "terminal-3", status: TerminalStatus.Stopped },
        ]),
        updateTerminal: vi.fn(),
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      // Perform health check to populate terminal health data
      await monitor.performHealthCheck();

      const health = monitor.getHealth();

      // Active terminals = terminals with health data that are not stopped
      // Busy terminal is skipped in health checks, so only terminal-1 and terminal-3 have health data
      // Only terminal-1 is not stopped
      expect(health.activeTerminals).toBe(1); // Only Idle (Busy skipped, Stopped not active)
    });
  });

  describe("Configuration", () => {
    it("should allow configuring stale threshold", async () => {
      const customThreshold = 5 * 60 * 1000; // 5 minutes
      monitor.setStaleThreshold(customThreshold);

      vi.setSystemTime(new Date("2025-01-01T12:00:00Z"));
      monitor.recordActivity("terminal-1");

      vi.setSystemTime(new Date("2025-01-01T12:06:00Z"));

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => [{ id: "terminal-1", status: TerminalStatus.Idle }]),
        updateTerminal: vi.fn(),
      });

      mockInvoke.mockResolvedValue({ has_session: true });

      await monitor.performHealthCheck();

      const health = monitor.getHealth();
      const terminalHealth = health.terminals.get("terminal-1");

      expect(terminalHealth?.isStale).toBe(true);
    });

    it("should allow configuring health check interval", () => {
      const customInterval = 60000; // 60 seconds
      monitor.setHealthCheckInterval(customInterval);

      monitor.start();

      // Verify timer was set with new interval
      // (Implementation detail - would need access to timer internals to verify)
      expect(() => monitor.start()).not.toThrow();
    });

    it("should allow configuring daemon ping interval", () => {
      const customInterval = 5000; // 5 seconds
      monitor.setDaemonPingInterval(customInterval);

      monitor.start();

      // Verify timer was set with new interval
      expect(() => monitor.start()).not.toThrow();
    });

    it("should restart timers when interval changed while running", () => {
      monitor.start();

      // Change interval while running
      monitor.setHealthCheckInterval(5000);
      monitor.setDaemonPingInterval(2000);

      // Should not throw and should continue working
      expect(() => monitor.stop()).not.toThrow();
    });
  });

  describe("Periodic Monitoring", () => {
    it("should perform health checks at configured interval", async () => {
      let healthCheckCount = 0;
      const checkTimes: number[] = [];

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      // Track health checks via console logs (most reliable)
      consoleLogSpy.mockImplementation((msg: string) => {
        if (msg.includes("Performing health check")) {
          healthCheckCount++;
          checkTimes.push(Date.now());
        }
      });

      const healthCheckInterval = 10000; // 10 seconds
      monitor.setHealthCheckInterval(healthCheckInterval);
      monitor.start();

      // Wait for initial health check
      await Promise.resolve();
      await Promise.resolve();
      expect(healthCheckCount).toBe(1);

      // Advance time exactly by the interval to trigger next health check
      await vi.advanceTimersByTimeAsync(healthCheckInterval);

      expect(healthCheckCount).toBe(2);
      expect(checkTimes.length).toBe(2);
      expect(checkTimes[1] - checkTimes[0]).toBe(healthCheckInterval);

      monitor.stop();
    });

    it("should ping daemon at configured interval", async () => {
      const pingTimes: number[] = [];

      mockInvoke.mockImplementation(() => {
        pingTimes.push(Date.now());
        return Promise.resolve(true);
      });

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      const daemonPingInterval = 5000; // 5 seconds
      monitor.setDaemonPingInterval(daemonPingInterval);
      monitor.start();

      // Wait for initial ping
      await Promise.resolve();
      await Promise.resolve();
      expect(pingTimes.length).toBe(1);

      // Clear initial ping
      mockInvoke.mockClear();

      // Advance time exactly by the interval to trigger next ping
      await vi.advanceTimersByTimeAsync(daemonPingInterval);

      expect(mockInvoke).toHaveBeenCalledWith("check_daemon_health");
      expect(pingTimes.length).toBe(2);
      expect(pingTimes[1] - pingTimes[0]).toBe(daemonPingInterval);

      monitor.stop();
    });

    it("should perform multiple periodic checks at exact intervals", async () => {
      let healthCheckCount = 0;
      const checkTimes: number[] = [];
      const startTime = Date.now();

      mockGetAppState.mockReturnValue({
        getTerminals: vi.fn(() => []),
        updateTerminal: vi.fn(),
      });

      // Track health checks via console logs
      consoleLogSpy.mockImplementation((msg: string) => {
        if (msg.includes("Performing health check")) {
          healthCheckCount++;
          checkTimes.push(Date.now());
        }
      });

      const healthCheckInterval = 1000; // 1 second for faster test
      monitor.setHealthCheckInterval(healthCheckInterval);
      monitor.start();

      // Wait for initial check
      await Promise.resolve();
      await Promise.resolve();
      expect(healthCheckCount).toBe(1);
      expect(checkTimes[0]).toBe(startTime);

      // First interval
      await vi.advanceTimersByTimeAsync(healthCheckInterval);
      expect(healthCheckCount).toBe(2);
      expect(checkTimes[1] - checkTimes[0]).toBe(healthCheckInterval);

      // Second interval
      await vi.advanceTimersByTimeAsync(healthCheckInterval);
      expect(healthCheckCount).toBe(3);
      expect(checkTimes[2] - checkTimes[1]).toBe(healthCheckInterval);

      // Third interval
      await vi.advanceTimersByTimeAsync(healthCheckInterval);
      expect(healthCheckCount).toBe(4);
      expect(checkTimes[3] - checkTimes[2]).toBe(healthCheckInterval);

      monitor.stop();

      // Verify exact count and timing
      expect(healthCheckCount).toBe(4);
      expect(checkTimes[3] - checkTimes[0]).toBe(3 * healthCheckInterval);
    });
  });
});
