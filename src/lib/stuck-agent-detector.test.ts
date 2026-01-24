/**
 * Tests for stuck-agent-detector.ts
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppState, setAppState } from "./state";
import {
  DEFAULT_STUCK_THRESHOLDS,
  getStuckAgentDetector,
  ROLE_DEFAULT_THRESHOLDS,
  StuckAgentDetector,
} from "./stuck-agent-detector";
import { TerminalStatus } from "./types";

// Mock the health monitor - use unknown to avoid strict type checking on mock
vi.mock("./health-monitor", () => ({
  getHealthMonitor: vi.fn(
    () =>
      ({
        getLastActivity: vi.fn(() => Date.now() - 60000), // 1 minute ago by default
      }) as unknown
  ),
}));

// Mock the terminal state parser
vi.mock("./terminal-state-parser", () => ({
  detectTerminalState: vi.fn(() =>
    Promise.resolve({
      type: "claude-code",
      status: "working",
      raw: "",
    })
  ),
}));

describe("StuckAgentDetector", () => {
  let detector: StuckAgentDetector;
  let appState: AppState;

  beforeEach(() => {
    // Create fresh app state for each test
    appState = new AppState();
    setAppState(appState);

    // Create fresh detector instance
    detector = new StuckAgentDetector();
  });

  afterEach(() => {
    detector.stop();
    vi.clearAllMocks();
  });

  describe("getThresholdsForRole", () => {
    it("should return default thresholds for unknown role", () => {
      const thresholds = detector.getThresholdsForRole("unknown-role");
      expect(thresholds).toEqual(DEFAULT_STUCK_THRESHOLDS);
    });

    it("should return default thresholds for undefined role", () => {
      const thresholds = detector.getThresholdsForRole(undefined);
      expect(thresholds).toEqual(DEFAULT_STUCK_THRESHOLDS);
    });

    it("should return builder-specific thresholds", () => {
      const thresholds = detector.getThresholdsForRole("builder");
      expect(thresholds.maxNoOutput).toBe(30 * 60 * 1000); // 30 minutes
      expect(thresholds.maxNeedsInput).toBe(5 * 60 * 1000); // 5 minutes
    });

    it("should return judge-specific thresholds", () => {
      const thresholds = detector.getThresholdsForRole("judge");
      expect(thresholds.maxNoOutput).toBe(10 * 60 * 1000); // 10 minutes
      expect(thresholds.maxNeedsInput).toBe(3 * 60 * 1000); // 3 minutes
    });

    it("should return champion-specific thresholds (shortest)", () => {
      const thresholds = detector.getThresholdsForRole("champion");
      expect(thresholds.maxNoOutput).toBe(5 * 60 * 1000); // 5 minutes
      expect(thresholds.maxNeedsInput).toBe(2 * 60 * 1000); // 2 minutes
    });

    it("should return shepherd-specific thresholds (longest)", () => {
      const thresholds = detector.getThresholdsForRole("shepherd");
      expect(thresholds.maxNoOutput).toBe(45 * 60 * 1000); // 45 minutes
      expect(thresholds.maxNeedsInput).toBe(5 * 60 * 1000);
    });

    it("should be case-insensitive for role names", () => {
      const thresholds1 = detector.getThresholdsForRole("Builder");
      const thresholds2 = detector.getThresholdsForRole("BUILDER");
      const thresholds3 = detector.getThresholdsForRole("builder");

      expect(thresholds1.maxNoOutput).toBe(thresholds2.maxNoOutput);
      expect(thresholds2.maxNoOutput).toBe(thresholds3.maxNoOutput);
    });
  });

  describe("analyzeTerminal", () => {
    it("should return not stuck for non-existent terminal", async () => {
      const analysis = await detector.analyzeTerminal("non-existent");

      expect(analysis.isStuck).toBe(false);
      expect(analysis.confidence).toBe("low");
      expect(analysis.recommendedAction).toBe("none");
      expect(analysis.reason).toBe("Terminal not found");
    });

    it("should return not stuck for healthy terminal with recent activity", async () => {
      // Add a terminal with recent activity
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      const analysis = await detector.analyzeTerminal("terminal-1");

      expect(analysis.isStuck).toBe(false);
      expect(analysis.recommendedAction).toBe("none");
    });

    it("should detect stuck terminal with no output exceeding threshold", async () => {
      // Mock health monitor to return old activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 35 * 60 * 1000), // 35 minutes ago
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      const analysis = await detector.analyzeTerminal("terminal-1");

      expect(analysis.isStuck).toBe(true);
      expect(analysis.reason).toContain("No output for");
    });

    it("should detect stuck terminal waiting for input too long", async () => {
      // Mock terminal state to show waiting for input
      const { detectTerminalState } = await import("./terminal-state-parser");
      vi.mocked(detectTerminalState).mockResolvedValue({
        type: "claude-code",
        status: "waiting-input",
        raw: "",
      });

      // Mock health monitor with recent activity (so no output duration doesn't trigger)
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 60000), // 1 minute ago
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      // First analysis starts tracking needs_input state
      await detector.analyzeTerminal("terminal-1");

      // Manually set the needs_input start time to exceed threshold
      const state = detector.getTerminalStuckState("terminal-1");
      if (state) {
        state.needsInputSince = Date.now() - 10 * 60 * 1000; // 10 minutes ago
      }

      const analysis = await detector.analyzeTerminal("terminal-1");

      expect(analysis.isStuck).toBe(true);
      expect(analysis.reason).toContain("Waiting for input");
    });

    it("should use role-specific thresholds for detection", async () => {
      // Mock health monitor with 8-minute-old activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 8 * 60 * 1000), // 8 minutes ago
      } as unknown as ReturnType<typeof getHealthMonitor>);

      // Champion has 5-minute threshold, should be stuck
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Champion",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "champion",
      });

      const championAnalysis = await detector.analyzeTerminal("terminal-1");
      expect(championAnalysis.isStuck).toBe(true);

      // Builder has 30-minute threshold, should not be stuck
      appState.terminals.addTerminal({
        id: "terminal-2",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      const builderAnalysis = await detector.analyzeTerminal("terminal-2");
      expect(builderAnalysis.isStuck).toBe(false);
    });
  });

  describe("checkAllTerminals", () => {
    it("should skip stopped terminals", async () => {
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Stopped",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        role: "builder",
      });

      const results = await detector.checkAllTerminals();

      expect(results.size).toBe(0);
    });

    it("should skip terminals without roles (plain shells)", async () => {
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Shell",
        status: TerminalStatus.Idle,
        isPrimary: false,
        // No role = plain shell
      });

      const results = await detector.checkAllTerminals();

      expect(results.size).toBe(0);
    });

    it("should check all active terminals with roles", async () => {
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      appState.terminals.addTerminal({
        id: "terminal-2",
        name: "Judge",
        status: TerminalStatus.Busy,
        isPrimary: false,
        role: "judge",
      });

      const results = await detector.checkAllTerminals();

      expect(results.size).toBe(2);
      expect(results.has("terminal-1")).toBe(true);
      expect(results.has("terminal-2")).toBe(true);
    });
  });

  describe("callbacks", () => {
    it("should register and unregister callbacks", () => {
      const callback = vi.fn();

      const unsubscribe = detector.onStuckDetected(callback);
      expect(typeof unsubscribe).toBe("function");

      unsubscribe();
      // Callback should be removed - no way to verify directly,
      // but shouldn't throw
    });

    it("should notify callbacks when terminal becomes stuck", async () => {
      const callback = vi.fn();
      detector.onStuckDetected(callback);

      // Mock health monitor to return old activity (trigger stuck)
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 35 * 60 * 1000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      await detector.analyzeTerminal("terminal-1");

      expect(callback).toHaveBeenCalledWith(
        "terminal-1",
        expect.objectContaining({
          isStuck: true,
          terminalId: "terminal-1",
        })
      );
    });

    it("should respect notification cooldown", async () => {
      const callback = vi.fn();
      detector.onStuckDetected(callback);

      // Mock health monitor to return old activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 35 * 60 * 1000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      // First analysis should notify
      await detector.analyzeTerminal("terminal-1");
      expect(callback).toHaveBeenCalledTimes(1);

      // Second analysis immediately after should not notify (cooldown)
      await detector.analyzeTerminal("terminal-1");
      expect(callback).toHaveBeenCalledTimes(1);
    });
  });

  describe("clearTerminalState", () => {
    it("should clear tracked state for terminal", async () => {
      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      // Analyze to create state
      await detector.analyzeTerminal("terminal-1");
      expect(detector.getTerminalStuckState("terminal-1")).toBeDefined();

      // Clear state
      detector.clearTerminalState("terminal-1");
      expect(detector.getTerminalStuckState("terminal-1")).toBeUndefined();
    });
  });

  describe("start/stop", () => {
    it("should start and stop without errors", () => {
      expect(() => detector.start()).not.toThrow();
      expect(() => detector.stop()).not.toThrow();
    });

    it("should not start twice", () => {
      detector.start();
      detector.start(); // Should warn but not throw
      detector.stop();
    });

    it("should be safe to stop when not running", () => {
      detector.stop(); // Should not throw
    });
  });

  describe("confidence and action recommendations", () => {
    it("should recommend notify for medium confidence stuck", async () => {
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 35 * 60 * 1000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      const analysis = await detector.analyzeTerminal("terminal-1");

      expect(analysis.isStuck).toBe(true);
      expect(analysis.confidence).toBe("medium");
      expect(analysis.recommendedAction).toBe("notify");
    });

    it("should increase confidence with multiple signals", async () => {
      // Mock terminal state to show waiting for input
      const { detectTerminalState } = await import("./terminal-state-parser");
      vi.mocked(detectTerminalState).mockResolvedValue({
        type: "claude-code",
        status: "waiting-input",
        raw: "",
      });

      // Mock health monitor with old activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 35 * 60 * 1000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });

      // First analysis
      await detector.analyzeTerminal("terminal-1");

      // Set needs_input to exceed threshold
      const state = detector.getTerminalStuckState("terminal-1");
      if (state) {
        state.needsInputSince = Date.now() - 10 * 60 * 1000;
      }

      const analysis = await detector.analyzeTerminal("terminal-1");

      // Both no output AND needs input exceeded = high confidence
      expect(analysis.isStuck).toBe(true);
      expect(analysis.confidence).toBe("high");
      expect(analysis.recommendedAction).toBe("restart");
    });
  });

  describe("ROLE_DEFAULT_THRESHOLDS", () => {
    it("should have thresholds for all standard roles", () => {
      const standardRoles = [
        "builder",
        "judge",
        "curator",
        "champion",
        "doctor",
        "architect",
        "hermit",
        "guide",
        "shepherd",
        "loom",
      ];

      for (const role of standardRoles) {
        expect(ROLE_DEFAULT_THRESHOLDS[role]).toBeDefined();
        expect(ROLE_DEFAULT_THRESHOLDS[role].maxNoOutput).toBeGreaterThan(0);
      }
    });

    it("should have champion with shortest thresholds", () => {
      const championThresholds = ROLE_DEFAULT_THRESHOLDS.champion;

      for (const [role, thresholds] of Object.entries(ROLE_DEFAULT_THRESHOLDS)) {
        if (role !== "champion") {
          expect(thresholds.maxNoOutput).toBeGreaterThanOrEqual(
            championThresholds.maxNoOutput || DEFAULT_STUCK_THRESHOLDS.maxNoOutput
          );
        }
      }
    });

    it("should have shepherd with longest thresholds", () => {
      const shepherdThresholds = ROLE_DEFAULT_THRESHOLDS.shepherd;

      for (const [role, thresholds] of Object.entries(ROLE_DEFAULT_THRESHOLDS)) {
        if (role !== "shepherd") {
          expect(thresholds.maxNoOutput).toBeLessThanOrEqual(
            shepherdThresholds.maxNoOutput || DEFAULT_STUCK_THRESHOLDS.maxNoOutput
          );
        }
      }
    });
  });
});

describe("getStuckAgentDetector singleton", () => {
  it("should return the same instance", () => {
    const instance1 = getStuckAgentDetector();
    const instance2 = getStuckAgentDetector();

    expect(instance1).toBe(instance2);
  });
});
