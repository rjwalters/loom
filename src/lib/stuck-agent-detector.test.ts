/**
 * Tests for stuck-agent-detector.ts
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppState, setAppState } from "./state";
import {
  DEFAULT_STUCK_THRESHOLDS,
  detectProgress,
  getStuckAgentDetector,
  hashChunk,
  isSimilarChunk,
  normalizeForHash,
  type OutputChunk,
  PROGRESS_PATTERNS,
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

// Mock the interval prompt manager
vi.mock("./interval-prompt-manager", () => ({
  getIntervalPromptManager: vi.fn(() => ({
    getStatus: vi.fn(() => null),
  })),
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

describe("Phase 2a: Pattern Detection", () => {
  describe("normalizeForHash", () => {
    it("should remove ISO timestamps", () => {
      const input = "Output at 2025-01-23T10:30:00Z was generated";
      const normalized = normalizeForHash(input);
      expect(normalized).not.toContain("2025-01-23T10:30:00Z");
      expect(normalized).toContain("Output at");
    });

    it("should remove date formats", () => {
      const input = "Created on 2025-01-23 successfully";
      const normalized = normalizeForHash(input);
      expect(normalized).not.toContain("2025-01-23");
    });

    it("should remove time formats", () => {
      const input = "Started at 10:30:00 and ended at 11:45:30";
      const normalized = normalizeForHash(input);
      expect(normalized).not.toContain("10:30:00");
      expect(normalized).not.toContain("11:45:30");
    });

    it("should normalize whitespace", () => {
      const input = "Multiple   spaces\n\nand   newlines";
      const normalized = normalizeForHash(input);
      expect(normalized).toBe("Multiple spaces and newlines");
    });
  });

  describe("hashChunk", () => {
    it("should return consistent hash for same input", () => {
      const input = "Hello, World!";
      const hash1 = hashChunk(input);
      const hash2 = hashChunk(input);
      expect(hash1).toBe(hash2);
    });

    it("should return different hashes for different content", () => {
      const hash1 = hashChunk("Hello, World!");
      const hash2 = hashChunk("Goodbye, World!");
      expect(hash1).not.toBe(hash2);
    });

    it("should normalize before hashing (timestamps don't affect hash)", () => {
      const hash1 = hashChunk("Output at 2025-01-23T10:00:00Z: data");
      const hash2 = hashChunk("Output at 2025-01-24T15:30:00Z: data");
      expect(hash1).toBe(hash2);
    });
  });

  describe("isSimilarChunk", () => {
    it("should return true for identical chunks", () => {
      const chunk1: OutputChunk = { hash: 12345, length: 100, timestamp: Date.now() };
      const chunk2: OutputChunk = { hash: 12345, length: 100, timestamp: Date.now() };
      expect(isSimilarChunk(chunk1, chunk2)).toBe(true);
    });

    it("should return false for different hashes", () => {
      const chunk1: OutputChunk = { hash: 12345, length: 100, timestamp: Date.now() };
      const chunk2: OutputChunk = { hash: 67890, length: 100, timestamp: Date.now() };
      expect(isSimilarChunk(chunk1, chunk2)).toBe(false);
    });

    it("should return true for same hash with similar lengths (within 80%)", () => {
      const chunk1: OutputChunk = { hash: 12345, length: 100, timestamp: Date.now() };
      const chunk2: OutputChunk = { hash: 12345, length: 85, timestamp: Date.now() };
      expect(isSimilarChunk(chunk1, chunk2)).toBe(true);
    });

    it("should return false for same hash with very different lengths", () => {
      const chunk1: OutputChunk = { hash: 12345, length: 100, timestamp: Date.now() };
      const chunk2: OutputChunk = { hash: 12345, length: 50, timestamp: Date.now() };
      expect(isSimilarChunk(chunk1, chunk2)).toBe(false);
    });
  });

  describe("recordOutput and pattern detection", () => {
    let detector: StuckAgentDetector;
    let appState: AppState;

    beforeEach(() => {
      appState = new AppState();
      setAppState(appState);
      detector = new StuckAgentDetector();

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });
    });

    afterEach(() => {
      detector.stop();
    });

    it("should accumulate output in chunks", () => {
      detector.recordOutput("terminal-1", "First output");
      const state = detector.getTerminalStuckState("terminal-1");
      expect(state?.patternState.currentChunkContent).toContain("First output");
    });

    it("should track progress when tool calls detected", () => {
      detector.recordOutput("terminal-1", "<function_calls>something</function_calls>");
      const state = detector.getTerminalStuckState("terminal-1");
      expect(state?.progressState.lastProgressTime).not.toBeNull();
      expect(state?.progressState.recentProgress.length).toBeGreaterThan(0);
    });

    it("should detect repeated patterns when threshold exceeded", async () => {
      // Mock health monitor with recent activity (so time-based detection doesn't trigger)
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 60000), // 1 minute ago
      } as unknown as ReturnType<typeof getHealthMonitor>);

      // First, initialize state by analyzing terminal
      await detector.analyzeTerminal("terminal-1");

      // Now manually populate chunks with identical content
      const state = detector.getTerminalStuckState("terminal-1");
      expect(state).toBeDefined();
      if (state) {
        const hash = hashChunk("Repeated content");
        const chunkLength = "Repeated content".length;
        // Add 4 identical chunks (exceeds default threshold of 3)
        state.patternState.chunks = [];
        for (let i = 0; i < 4; i++) {
          state.patternState.chunks.push({
            hash,
            length: chunkLength,
            timestamp: Date.now() - i * 30000,
          });
        }
      }

      const analysis = await detector.analyzeTerminal("terminal-1");
      expect(analysis.signals.repeatedPatterns).toBe(true);
      expect(analysis.isStuck).toBe(true);
      expect(analysis.reason).toContain("repeated output patterns");
    });
  });
});

describe("Phase 2b: Progress Tracking", () => {
  describe("PROGRESS_PATTERNS", () => {
    it("should detect function_calls tags", () => {
      expect(PROGRESS_PATTERNS.toolCall.test("<function_calls>")).toBe(true);
      expect(PROGRESS_PATTERNS.toolCall.test("regular text")).toBe(false);
    });

    it("should detect function_results tags", () => {
      expect(PROGRESS_PATTERNS.toolResult.test("<function_results>")).toBe(true);
      expect(PROGRESS_PATTERNS.toolResult.test("regular text")).toBe(false);
    });

    it("should detect git operations", () => {
      expect(PROGRESS_PATTERNS.gitOp.test("git push origin main")).toBe(true);
      expect(PROGRESS_PATTERNS.gitOp.test("git commit -m 'message'")).toBe(true);
      expect(PROGRESS_PATTERNS.gitOp.test("git status")).toBe(false);
    });

    it("should detect gh operations", () => {
      expect(PROGRESS_PATTERNS.ghOp.test("gh pr create")).toBe(true);
      expect(PROGRESS_PATTERNS.ghOp.test("gh issue edit 123")).toBe(true);
      expect(PROGRESS_PATTERNS.ghOp.test("gh pr list")).toBe(false);
    });
  });

  describe("detectProgress", () => {
    it("should return true for tool calls", () => {
      expect(detectProgress("<function_calls>something")).toBe(true);
    });

    it("should return true for tool results", () => {
      expect(detectProgress("<function_results>something")).toBe(true);
    });

    it("should return false for regular text", () => {
      expect(detectProgress("Just some regular text output")).toBe(false);
    });
  });

  describe("prompt without progress detection", () => {
    let detector: StuckAgentDetector;
    let appState: AppState;

    beforeEach(() => {
      appState = new AppState();
      setAppState(appState);
      detector = new StuckAgentDetector();

      appState.terminals.addTerminal({
        id: "terminal-1",
        name: "Builder",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "builder",
      });
    });

    afterEach(() => {
      detector.stop();
    });

    it("should record prompt sent time", () => {
      detector.recordPromptSent("terminal-1");
      const state = detector.getTerminalStuckState("terminal-1");
      expect(state?.progressState.lastPromptTime).not.toBeNull();
    });

    it("should not flag prompt without progress when progress detected after prompt", async () => {
      // Mock health monitor with recent activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 60000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      detector.recordPromptSent("terminal-1");

      // Simulate progress after prompt
      detector.recordOutput("terminal-1", "<function_calls>tool call</function_calls>");

      const analysis = await detector.analyzeTerminal("terminal-1");
      expect(analysis.signals.promptWithoutProgress).toBe(false);
    });

    it("should flag prompt without progress when timeout exceeded", async () => {
      // Mock health monitor with recent activity
      const { getHealthMonitor } = await import("./health-monitor");
      vi.mocked(getHealthMonitor).mockReturnValue({
        getLastActivity: vi.fn(() => Date.now() - 60000),
      } as unknown as ReturnType<typeof getHealthMonitor>);

      // Initialize state first
      await detector.analyzeTerminal("terminal-1");

      // Set prompt time to exceed timeout (10 minutes for builder)
      const state = detector.getTerminalStuckState("terminal-1");
      if (state) {
        state.progressState.lastPromptTime = Date.now() - 15 * 60 * 1000; // 15 minutes ago
        state.progressState.lastProgressTime = null; // No progress
      }

      const analysis = await detector.analyzeTerminal("terminal-1");
      expect(analysis.signals.promptWithoutProgress).toBe(true);
      expect(analysis.isStuck).toBe(true);
      expect(analysis.reason).toContain("No tool calls after prompt");
    });
  });
});

describe("Phase 2 role-specific thresholds", () => {
  it("should have chunkWindowSeconds for all roles", () => {
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
      expect(ROLE_DEFAULT_THRESHOLDS[role].chunkWindowSeconds).toBeDefined();
      expect(ROLE_DEFAULT_THRESHOLDS[role].chunkWindowSeconds).toBeGreaterThan(0);
    }
  });

  it("should have noProgressTimeout for all roles", () => {
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
      expect(ROLE_DEFAULT_THRESHOLDS[role].noProgressTimeout).toBeDefined();
      expect(ROLE_DEFAULT_THRESHOLDS[role].noProgressTimeout).toBeGreaterThan(0);
    }
  });

  it("should have champion with shortest noProgressTimeout", () => {
    const championTimeout = ROLE_DEFAULT_THRESHOLDS.champion.noProgressTimeout || Infinity;

    for (const [role, thresholds] of Object.entries(ROLE_DEFAULT_THRESHOLDS)) {
      if (role !== "champion") {
        const timeout = thresholds.noProgressTimeout || DEFAULT_STUCK_THRESHOLDS.noProgressTimeout;
        expect(timeout).toBeGreaterThanOrEqual(championTimeout);
      }
    }
  });

  it("should have shepherd with longest noProgressTimeout", () => {
    const shepherdTimeout = ROLE_DEFAULT_THRESHOLDS.shepherd.noProgressTimeout || 0;

    for (const [role, thresholds] of Object.entries(ROLE_DEFAULT_THRESHOLDS)) {
      if (role !== "shepherd") {
        const timeout = thresholds.noProgressTimeout || DEFAULT_STUCK_THRESHOLDS.noProgressTimeout;
        expect(timeout).toBeLessThanOrEqual(shepherdTimeout);
      }
    }
  });
});

describe("getStuckAgentDetector singleton", () => {
  it("should return the same instance", () => {
    const instance1 = getStuckAgentDetector();
    const instance2 = getStuckAgentDetector();

    expect(instance1).toBe(instance2);
  });
});
