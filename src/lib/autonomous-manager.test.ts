import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as agentLauncher from "./agent-launcher";
import { getAutonomousManager } from "./autonomous-manager";
import { AppState, TerminalStatus } from "./state";

// Mock agent-launcher
vi.mock("./agent-launcher", () => ({
  sendPromptToAgent: vi.fn(),
}));

describe("autonomous-manager", () => {
  let manager: ReturnType<typeof getAutonomousManager>;
  let state: AppState;

  beforeEach(() => {
    // Use fake timers to control setInterval
    vi.useFakeTimers();

    // Get fresh manager instance
    manager = getAutonomousManager();

    // Stop any existing intervals
    manager.stopAll();

    // Clear mocks
    vi.clearAllMocks();

    // Create fresh state
    state = new AppState();
  });

  afterEach(() => {
    // Clean up intervals
    manager.stopAll();

    // Restore real timers
    vi.useRealTimers();
  });

  describe("startAutonomous", () => {
    it("should start autonomous mode for terminal with valid config", () => {
      const terminal = {
        id: "test-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Continue working",
        },
      };

      manager.startAutonomous(terminal);

      expect(manager.isAutonomous("test-1")).toBe(true);
    });

    it("should not start autonomous mode without targetInterval", () => {
      const terminal = {
        id: "test-2",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          intervalPrompt: "Continue working",
        },
      };

      manager.startAutonomous(terminal);

      expect(manager.isAutonomous("test-2")).toBe(false);
    });

    it("should not start autonomous mode with zero interval", () => {
      const terminal = {
        id: "test-3",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 0,
          intervalPrompt: "Continue working",
        },
      };

      manager.startAutonomous(terminal);

      expect(manager.isAutonomous("test-3")).toBe(false);
    });

    it("should not start autonomous mode with negative interval", () => {
      const terminal = {
        id: "test-4",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: -1000,
          intervalPrompt: "Continue working",
        },
      };

      manager.startAutonomous(terminal);

      expect(manager.isAutonomous("test-4")).toBe(false);
    });

    it("should send prompt at interval", async () => {
      const terminal = {
        id: "test-5",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work on tasks",
        },
      };

      manager.startAutonomous(terminal);

      // Fast forward time by 1000ms (one interval)
      await vi.advanceTimersByTimeAsync(1000);

      // Check that sendPromptToAgent was called (uses sessionId for IPC)
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledWith("test-5", "Work on tasks");
    });

    it("should send prompt multiple times at interval", async () => {
      const terminal = {
        id: "test-6",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 500,
          intervalPrompt: "Continue",
        },
      };

      manager.startAutonomous(terminal);

      // Advance by 3 intervals
      await vi.advanceTimersByTimeAsync(1500);

      // Should have been called 3 times
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(3);
    });

    it("should use default prompt when not specified", async () => {
      const terminal = {
        id: "test-7",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
        },
      };

      manager.startAutonomous(terminal);

      await vi.advanceTimersByTimeAsync(1000);

      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledWith("test-7", "Continue working");
    });

    it("should update lastRun timestamp after each prompt", async () => {
      const terminal = {
        id: "test-8",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      const status1 = manager.getStatus("test-8");
      const initialLastRun = status1?.lastRun;

      // Advance time
      await vi.advanceTimersByTimeAsync(1000);

      const status2 = manager.getStatus("test-8");
      const updatedLastRun = status2?.lastRun;

      expect(updatedLastRun).toBeGreaterThan(initialLastRun || 0);
    });

    it("should stop existing interval when starting new one", async () => {
      const terminal = {
        id: "test-9",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "First prompt",
        },
      };

      // Start first interval
      manager.startAutonomous(terminal);

      // Start second interval with different config
      terminal.roleConfig.intervalPrompt = "Second prompt";
      manager.startAutonomous(terminal);

      // Advance time
      await vi.advanceTimersByTimeAsync(1000);

      // Should only receive the new prompt, not the old one
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledWith("test-9", "Second prompt");
    });

    it("should handle errors from sendPromptToAgent gracefully", async () => {
      vi.mocked(agentLauncher.sendPromptToAgent).mockRejectedValue(new Error("Send failed"));

      const terminal = {
        id: "test-10",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // Advance time - should not throw
      await vi.advanceTimersByTimeAsync(1000);

      // Interval should still be active despite error
      expect(manager.isAutonomous("test-10")).toBe(true);
    });
  });

  describe("stopAutonomous", () => {
    it("should stop autonomous mode for terminal", async () => {
      const terminal = {
        id: "test-11",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);
      expect(manager.isAutonomous("test-11")).toBe(true);

      manager.stopAutonomous("test-11");
      expect(manager.isAutonomous("test-11")).toBe(false);

      // Advance time - no prompts should be sent
      await vi.advanceTimersByTimeAsync(1000);
      expect(agentLauncher.sendPromptToAgent).not.toHaveBeenCalled();
    });

    it("should not error when stopping non-existent interval", () => {
      expect(() => manager.stopAutonomous("non-existent")).not.toThrow();
    });

    it("should clear interval properly", async () => {
      const terminal = {
        id: "test-12",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // Let one interval fire
      await vi.advanceTimersByTimeAsync(1000);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Stop and advance time
      manager.stopAutonomous("test-12");
      await vi.advanceTimersByTimeAsync(2000);

      // Should not have been called again
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);
    });
  });

  describe("isAutonomous", () => {
    it("should return false for terminal without autonomous mode", () => {
      expect(manager.isAutonomous("non-existent")).toBe(false);
    });

    it("should return true for terminal with autonomous mode", () => {
      const terminal = {
        id: "test-13",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);
      expect(manager.isAutonomous("test-13")).toBe(true);
    });
  });

  describe("restartAutonomous", () => {
    it("should restart autonomous mode with new config", async () => {
      const terminal = {
        id: "test-14",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Old prompt",
        },
      };

      manager.startAutonomous(terminal);

      // Update config
      terminal.roleConfig.intervalPrompt = "New prompt";
      manager.restartAutonomous(terminal);

      // Advance time
      await vi.advanceTimersByTimeAsync(1000);

      // Should use new prompt
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledWith("test-14", "New prompt");
    });

    it("should handle restart when not already running", async () => {
      const terminal = {
        id: "test-15",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      await expect(manager.restartAutonomous(terminal)).resolves.not.toThrow();
      expect(manager.isAutonomous("test-15")).toBe(true);
    });
  });

  describe("startAllAutonomous", () => {
    it("should start autonomous mode for all eligible terminals", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Autonomous 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work 1",
        },
      });

      state.addTerminal({
        id: "terminal-2",
        name: "Manual",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 0,
          intervalPrompt: "Work 2",
        },
      });

      state.addTerminal({
        id: "terminal-3",
        name: "Autonomous 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "reviewer",
        roleConfig: {
          targetInterval: 5000,
          intervalPrompt: "Review",
        },
      });

      manager.startAllAutonomous(state);

      expect(manager.isAutonomous("terminal-1")).toBe(true);
      expect(manager.isAutonomous("terminal-2")).toBe(false);
      expect(manager.isAutonomous("terminal-3")).toBe(true);
    });

    it("should skip terminals without role", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "No Role",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      manager.startAllAutonomous(state);

      expect(manager.isAutonomous("terminal-1")).toBe(false);
    });

    it("should handle empty state", () => {
      expect(() => manager.startAllAutonomous(state)).not.toThrow();
    });
  });

  describe("stopAll", () => {
    it("should stop all autonomous intervals", () => {
      const terminal1 = {
        id: "test-16",
        name: "Test 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work 1",
        },
      };

      const terminal2 = {
        id: "test-17",
        name: "Test 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "reviewer",
        roleConfig: {
          targetInterval: 2000,
          intervalPrompt: "Review",
        },
      };

      manager.startAutonomous(terminal1);
      manager.startAutonomous(terminal2);

      expect(manager.isAutonomous("test-16")).toBe(true);
      expect(manager.isAutonomous("test-17")).toBe(true);

      manager.stopAll();

      expect(manager.isAutonomous("test-16")).toBe(false);
      expect(manager.isAutonomous("test-17")).toBe(false);
    });

    it("should handle no active intervals", () => {
      expect(() => manager.stopAll()).not.toThrow();
    });
  });

  describe("getStatus", () => {
    it("should return status for autonomous terminal", () => {
      const terminal = {
        id: "test-18",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      const status = manager.getStatus("test-18");

      expect(status).toBeDefined();
      expect(status?.terminalId).toBe("test-18");
      expect(status?.targetInterval).toBe(1000);
      expect(status?.lastRun).toBeGreaterThan(0);
    });

    it("should return undefined for non-autonomous terminal", () => {
      const status = manager.getStatus("non-existent");
      expect(status).toBeUndefined();
    });
  });

  describe("getAllStatus", () => {
    it("should return all active autonomous intervals", () => {
      const terminal1 = {
        id: "test-19",
        name: "Test 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 1000,
          intervalPrompt: "Work 1",
        },
      };

      const terminal2 = {
        id: "test-20",
        name: "Test 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "reviewer",
        roleConfig: {
          targetInterval: 2000,
          intervalPrompt: "Review",
        },
      };

      manager.startAutonomous(terminal1);
      manager.startAutonomous(terminal2);

      const statuses = manager.getAllStatus();

      expect(statuses).toHaveLength(2);
      expect(statuses.map((s) => s.terminalId)).toContain("test-19");
      expect(statuses.map((s) => s.terminalId)).toContain("test-20");
    });

    it("should return empty array when no autonomous terminals", () => {
      const statuses = manager.getAllStatus();
      expect(statuses).toEqual([]);
    });
  });

  describe("Overrun Protection", () => {
    it("should prevent overlapping executions", async () => {
      // Create a promise that we can manually resolve
      let resolveExecution: (() => void) | null = null;
      const executionPromise = new Promise<void>((resolve) => {
        resolveExecution = resolve;
      });

      // Mock sendPromptToAgent to return a long-running promise
      vi.mocked(agentLauncher.sendPromptToAgent).mockReturnValue(executionPromise);

      const terminal = {
        id: "test-overrun-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 100, // 100ms interval
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // First execution should start immediately with first interval tick
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Second interval tick should be skipped (first execution still in progress)
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Third interval tick should also be skipped
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Complete the first execution
      resolveExecution!();
      await executionPromise;

      // Next interval tick should start a new execution
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(2);
    });

    it("should allow next execution after previous completes", async () => {
      let executionCount = 0;
      vi.mocked(agentLauncher.sendPromptToAgent).mockImplementation(async () => {
        executionCount++;
        // Fast execution (completes before next interval)
        await Promise.resolve();
      });

      const terminal = {
        id: "test-overrun-2",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 100,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // First execution
      await vi.advanceTimersByTimeAsync(100);
      expect(executionCount).toBe(1);

      // Second execution (first completed, so this should run)
      await vi.advanceTimersByTimeAsync(100);
      expect(executionCount).toBe(2);

      // Third execution
      await vi.advanceTimersByTimeAsync(100);
      expect(executionCount).toBe(3);
    });

    it("should clean up activePrompts on error", async () => {
      // Mock sendPromptToAgent to throw error
      vi.mocked(agentLauncher.sendPromptToAgent).mockRejectedValueOnce(new Error("Test error"));

      const terminal = {
        id: "test-overrun-3",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 100,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // First execution (will error)
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Reset mock to succeed
      vi.mocked(agentLauncher.sendPromptToAgent).mockResolvedValue();

      // Next execution should run (activePrompts was cleaned up despite error)
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(2);
    });

    it("should wait for active execution when stopping", async () => {
      let resolveExecution: (() => void) | null = null;
      const executionPromise = new Promise<void>((resolve) => {
        resolveExecution = resolve;
      });

      vi.mocked(agentLauncher.sendPromptToAgent).mockReturnValue(executionPromise);

      const terminal = {
        id: "test-overrun-4",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 100,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // Start first execution
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Try to stop (should wait for execution to complete)
      const stopPromise = manager.stopAutonomous(terminal.id);

      // Advance real time slightly to allow promise microtasks to run
      await vi.advanceTimersByTimeAsync(50);
      await Promise.resolve(); // Flush microtasks

      // Note: With fake timers, the while loop's setTimeout won't advance
      // So we need to complete the execution for stopAutonomous to finish
      resolveExecution!();
      await executionPromise;

      // Advance timers for the while loop's setTimeout checks
      await vi.advanceTimersByTimeAsync(200);

      // Now stopAutonomous should complete
      await stopPromise;
    });

    it("should prevent manual runNow from overlapping with interval", async () => {
      let resolveExecution: (() => void) | null = null;
      const executionPromise = new Promise<void>((resolve) => {
        resolveExecution = resolve;
      });

      vi.mocked(agentLauncher.sendPromptToAgent).mockReturnValue(executionPromise);

      const terminal = {
        id: "test-overrun-5",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        role: "worker",
        roleConfig: {
          targetInterval: 100,
          intervalPrompt: "Work",
        },
      };

      manager.startAutonomous(terminal);

      // Start first execution from interval
      await vi.advanceTimersByTimeAsync(100);
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Try to run manually while interval execution is in progress
      // This should be skipped due to overrun protection
      // Note: runNow calls restartAutonomous which is async, so we don't await it
      const runNowPromise = manager.runNow(terminal);

      // Should still be just 1 call (manual was skipped, restartAutonomous is waiting)
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(1);

      // Complete the execution so restartAutonomous can finish
      resolveExecution!();
      await executionPromise;
      await vi.advanceTimersByTimeAsync(200); // Let stopAutonomous while loop run
      await runNowPromise; // Wait for runNow to complete

      // Count should be 2:
      // 1. Original interval execution (which we resolved above)
      // 2. New interval after restart (runNow restarts the interval)
      // The manual execution itself was skipped due to overlap protection
      expect(agentLauncher.sendPromptToAgent).toHaveBeenCalledTimes(2);
    });
  });

  describe("singleton pattern", () => {
    it("should return same instance on multiple calls", () => {
      const instance1 = getAutonomousManager();
      const instance2 = getAutonomousManager();

      expect(instance1).toBe(instance2);
    });
  });
});
