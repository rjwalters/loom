import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getOutputPoller, OutputPoller } from "./output-poller";

// Mock Tauri API
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock terminal-manager module
vi.mock("./terminal-manager", () => ({
  getTerminalManager: vi.fn(),
}));

import { invoke } from "@tauri-apps/api/tauri";
import { getTerminalManager } from "./terminal-manager";

// Helper to assert JSON structured log messages
function assertLogContains(spy: { mock: { calls: unknown[][] } }, expectedSubstring: string) {
  const calls = spy.mock.calls;
  const found = calls.some((call: unknown[]) => {
    try {
      const log = JSON.parse(call[0] as string);
      return log.message?.includes(expectedSubstring);
    } catch {
      return false;
    }
  });
  expect(found, `Expected log containing: ${expectedSubstring}`).toBe(true);
}
describe("OutputPoller", () => {
  let poller: OutputPoller;
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  // Mock terminal manager
  const mockTerminalManager = {
    writeToTerminal: vi.fn(),
    clearAndWriteTerminal: vi.fn(),
  };

  // Helper to create base64-encoded output
  function encodeOutput(text: string): string {
    // Use TextEncoder to handle UTF-8 properly
    const encoder = new TextEncoder();
    const bytes = encoder.encode(text);
    const binaryString = Array.from(bytes, (byte) => String.fromCharCode(byte)).join("");
    return btoa(binaryString);
  }

  // Helper to create mock terminal output
  function createMockOutput(text: string, byteCount: number) {
    return {
      output: encodeOutput(text),
      byte_count: byteCount,
    };
  }

  beforeEach(() => {
    // Reset mocks
    vi.clearAllMocks();
    vi.useFakeTimers();

    // Setup console spies
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Setup mock implementations
    vi.mocked(getTerminalManager).mockReturnValue(mockTerminalManager as any);
    vi.mocked(invoke).mockResolvedValue({ output: "", byte_count: 0 });

    // Create fresh poller instance
    poller = new OutputPoller();
  });

  afterEach(() => {
    poller.stopAll();
    consoleLogSpy.mockRestore();
    consoleWarnSpy.mockRestore();
    consoleErrorSpy.mockRestore();
    vi.useRealTimers();
  });

  describe("Start and Stop Polling", () => {
    it("starts polling for a terminal", async () => {
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput("test output", 11));

      poller.startPolling("terminal-1");

      // Wait for initial poll
      await vi.runOnlyPendingTimersAsync();

      expect(invoke).toHaveBeenCalledWith("get_terminal_output", {
        id: "terminal-1",
        startByte: null, // First poll starts from beginning
      });

      expect(poller.isPolling("terminal-1")).toBe(true);
    });

    it("prevents starting multiple pollers for same terminal", () => {
      poller.startPolling("terminal-1");
      poller.startPolling("terminal-1");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Already polling terminal terminal-1");
    });

    it("stops polling and clears state", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      poller.stopPolling("terminal-1");

      expect(poller.isPolling("terminal-1")).toBe(false);
      expect(poller.getPollerCount()).toBe(0);
    });

    it("stops polling all terminals", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      poller.startPolling("terminal-2");
      await vi.runOnlyPendingTimersAsync();

      poller.stopAll();

      expect(poller.isPolling("terminal-1")).toBe(false);
      expect(poller.isPolling("terminal-2")).toBe(false);
      expect(poller.getPollerCount()).toBe(0);
    });

    it("handles stopping non-existent terminal gracefully", () => {
      poller.stopPolling("non-existent");
      // Should not throw or log error
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });
  });

  describe("Pause and Resume", () => {
    it("pauses polling while keeping state", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("test", 4));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      const invokeCallsBefore = vi.mocked(invoke).mock.calls.length;

      poller.pausePolling("terminal-1");

      // Advance time - should not poll while paused
      vi.advanceTimersByTime(1000);
      await vi.runOnlyPendingTimersAsync();

      const invokeCallsAfter = vi.mocked(invoke).mock.calls.length;
      expect(invokeCallsAfter).toBe(invokeCallsBefore); // No new calls

      // State should still exist
      expect(poller.getPollerCount()).toBe(1); // State kept but not polling
    });

    it("resumes polling from last byte count", async () => {
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValueOnce(createMockOutput("more", 11));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      poller.pausePolling("terminal-1");
      poller.resumePolling("terminal-1");

      await vi.runOnlyPendingTimersAsync();

      // Second poll should request from byte 7 (after "initial")
      const resumeCall = vi
        .mocked(invoke)
        .mock.calls.find((call) => call[1] && (call[1] as any).startByte === 7);
      expect(resumeCall).toBeDefined();
    });

    it("starts fresh polling if resume called without existing state", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("test", 4));

      poller.resumePolling("terminal-1");

      await vi.runOnlyPendingTimersAsync();

      expect(invoke).toHaveBeenCalledWith("get_terminal_output", {
        id: "terminal-1",
        startByte: null, // Fresh start
      });
    });

    it("warns if resume called while already actively polling", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      poller.resumePolling("terminal-1");

      expect(consoleWarnSpy).toHaveBeenCalledWith(
        "Terminal terminal-1 is already actively polling"
      );
    });

    it("handles pause on non-existent terminal gracefully", () => {
      poller.pausePolling("non-existent");
      // Should not throw
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });
  });

  describe("Output Retrieval and Writing", () => {
    it("decodes base64 output and writes to terminal", async () => {
      const text = "Hello, World!";
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput(text, text.length));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(mockTerminalManager.clearAndWriteTerminal).toHaveBeenCalledWith("terminal-1", text);
    });

    it("clears terminal on first poll", async () => {
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput("initial output", 14));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(mockTerminalManager.clearAndWriteTerminal).toHaveBeenCalledWith(
        "terminal-1",
        "initial output"
      );
      expect(mockTerminalManager.writeToTerminal).not.toHaveBeenCalled();
    });

    it("appends new output on subsequent polls", async () => {
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValueOnce(createMockOutput("more", 11));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Clear the clearAndWriteTerminal call
      mockTerminalManager.clearAndWriteTerminal.mockClear();

      // Trigger next poll
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      expect(mockTerminalManager.writeToTerminal).toHaveBeenCalledWith("terminal-1", "more");
      expect(mockTerminalManager.clearAndWriteTerminal).not.toHaveBeenCalled();
    });

    it("handles empty polls gracefully", async () => {
      vi.mocked(invoke).mockResolvedValue({ output: "", byte_count: 0 });

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Should not write to terminal on empty poll
      expect(mockTerminalManager.clearAndWriteTerminal).not.toHaveBeenCalled();
      expect(mockTerminalManager.writeToTerminal).not.toHaveBeenCalled();
    });

    it("updates byte count after each poll", async () => {
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("first", 5))
        .mockResolvedValueOnce(createMockOutput("second", 11));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Next poll should request from byte 5
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      const secondPollCall = vi
        .mocked(invoke)
        .mock.calls.find((call) => call[1] && (call[1] as any).startByte === 5);
      expect(secondPollCall).toBeDefined();
    });

    it("handles multi-byte UTF-8 characters correctly", async () => {
      const text = "Hello 世界 🌍";
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput(text, text.length));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(mockTerminalManager.clearAndWriteTerminal).toHaveBeenCalledWith("terminal-1", text);
    });
  });

  describe("Error Handling", () => {
    it("tracks consecutive errors", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("IPC timeout"));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // After initial poll, we have 1 error
      // Then scheduleNextPoll runs, which triggers another poll, giving us 2 errors
      const errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBeGreaterThanOrEqual(1);
      expect(errorState?.lastErrorTime).toBeGreaterThan(0);
    });

    it("resets error count on successful poll", async () => {
      vi.mocked(invoke)
        .mockRejectedValueOnce(new Error("First error"))
        .mockRejectedValueOnce(new Error("Second error")) // scheduleNextPoll triggers another
        .mockResolvedValue(createMockOutput("success", 7));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      let errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBeGreaterThanOrEqual(1);

      // Next poll succeeds
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBe(0);
      expect(errorState?.lastErrorTime).toBeNull();
    });

    it("stops polling after max consecutive errors", async () => {
      poller.setMaxConsecutiveErrors(3);
      vi.mocked(invoke).mockRejectedValue(new Error("Persistent error"));

      poller.startPolling("terminal-1");

      // Trigger 3 consecutive errors
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      expect(poller.isPolling("terminal-1")).toBe(false);
      expect(consoleErrorSpy).toHaveBeenCalledWith(
        "Stopping polling for terminal terminal-1 after 3 consecutive errors"
      );
    });

    it("calls error callback on max consecutive errors", async () => {
      const errorCallback = vi.fn();
      poller.onError(errorCallback);
      poller.setMaxConsecutiveErrors(2);

      vi.mocked(invoke).mockRejectedValue(new Error("Fatal error"));

      poller.startPolling("terminal-1");

      // Trigger 2 consecutive errors
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      expect(errorCallback).toHaveBeenCalledWith("terminal-1", "Fatal error");
    });

    it("logs errors only occasionally to avoid spam", async () => {
      poller.setMaxConsecutiveErrors(10); // Prevent auto-stop
      vi.mocked(invoke).mockRejectedValue(new Error("Spam error"));

      poller.startPolling("terminal-1");

      // First error - should log
      await vi.runOnlyPendingTimersAsync();

      const errorLogCount = consoleErrorSpy.mock.calls.filter((call) =>
        call.some((arg) => typeof arg === "string" && arg.includes("Error polling terminal"))
      ).length;

      // Should have logged first error (1 consecutive errors)
      expect(errorLogCount).toBeGreaterThanOrEqual(1);
    });

    it("returns null error state for non-existent terminal", () => {
      const errorState = poller.getErrorState("non-existent");
      expect(errorState).toBeNull();
    });

    it("handles non-Error objects in catch block", async () => {
      vi.mocked(invoke).mockRejectedValue("string error");

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      assertLogContains(consoleErrorSpy, "Error polling terminal");
    });
  });

  describe("Adaptive Polling Frequency", () => {
    it("starts with active polling interval", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Default active interval is 50ms
      const callsBefore = vi.mocked(invoke).mock.calls.length;

      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      const callsAfter = vi.mocked(invoke).mock.calls.length;
      expect(callsAfter).toBeGreaterThan(callsBefore);
    });

    it("slows down polling after activity timeout", async () => {
      // First poll has output
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValue(createMockOutput("", 7)); // Subsequent polls empty

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Advance past activity timeout (30s default)
      vi.advanceTimersByTime(31000);
      await vi.runOnlyPendingTimersAsync();

      // Should log frequency reduction
      const frequencyLogs = consoleLogSpy.mock.calls.filter((call) =>
        call.some(
          (arg) =>
            typeof arg === "string" &&
            arg.includes("Terminal terminal-1 idle for") &&
            arg.includes("reducing poll frequency to 10000ms")
        )
      );
      expect(frequencyLogs.length).toBeGreaterThan(0);
    });

    it("speeds up polling when activity resumes", async () => {
      // First poll has output, then no output, then output again
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValue(createMockOutput("", 7)); // Empty polls

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Wait for idle state
      vi.advanceTimersByTime(31000);
      await vi.runOnlyPendingTimersAsync();

      consoleLogSpy.mockClear();

      // New output arrives
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput("new output", 17));
      vi.advanceTimersByTime(10000); // Idle poll interval
      await vi.runOnlyPendingTimersAsync();

      // Should log frequency increase
      assertLogContains(consoleLogSpy, "increasing poll frequency");
    });

    it("maintains idle frequency for inactive terminals", async () => {
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValue(createMockOutput("", 7));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Wait for idle state
      vi.advanceTimersByTime(31000);
      await vi.runOnlyPendingTimersAsync();

      consoleLogSpy.mockClear();

      // Continue with no output - should stay at idle frequency
      vi.advanceTimersByTime(10000);
      await vi.runOnlyPendingTimersAsync();

      // Should not log frequency change (already idle)
      const frequencyChangeLogs = consoleLogSpy.mock.calls.filter((call) =>
        call.some((arg) => typeof arg === "string" && arg.includes("poll frequency"))
      );
      expect(frequencyChangeLogs.length).toBe(0);
    });
  });

  describe("Activity Callback", () => {
    it("calls activity callback when output is received", async () => {
      const activityCallback = vi.fn();
      poller.onActivity(activityCallback);

      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput("output", 6));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(activityCallback).toHaveBeenCalledWith("terminal-1");
    });

    it("does not call activity callback on empty polls", async () => {
      const activityCallback = vi.fn();
      poller.onActivity(activityCallback);

      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(activityCallback).not.toHaveBeenCalled();
    });

    it("calls activity callback for each terminal independently", async () => {
      const activityCallback = vi.fn();
      poller.onActivity(activityCallback);

      vi.mocked(invoke).mockResolvedValue(createMockOutput("output", 6));

      poller.startPolling("terminal-1");
      poller.startPolling("terminal-2");
      await vi.runOnlyPendingTimersAsync();

      expect(activityCallback).toHaveBeenCalledWith("terminal-1");
      expect(activityCallback).toHaveBeenCalledWith("terminal-2");
    });
  });

  describe("Configuration", () => {
    it("allows setting poll interval", () => {
      poller.setPollInterval(100);
      expect(poller.getPollInterval()).toBe(100);
    });

    it("restarts all pollers when interval changes", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      const callsBefore = vi.mocked(invoke).mock.calls.length;

      poller.setPollInterval(100);

      // Should have restarted polling
      await vi.runOnlyPendingTimersAsync();

      expect(vi.mocked(invoke).mock.calls.length).toBeGreaterThan(callsBefore);
    });

    it("allows setting max consecutive errors", async () => {
      poller.setMaxConsecutiveErrors(10);

      let callCount = 0;
      vi.mocked(invoke).mockImplementation(() => {
        callCount++;
        // Succeed after 5 errors to prevent reaching max
        if (callCount > 5) {
          return Promise.resolve(createMockOutput("success", 7));
        }
        return Promise.reject(new Error("error"));
      });

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Trigger a few more polls
      for (let i = 0; i < 3; i++) {
        vi.advanceTimersByTime(50);
        await vi.runOnlyPendingTimersAsync();
      }

      // Should still be polling (errors were reset by success)
      expect(poller.isPolling("terminal-1")).toBe(true);
    });
  });

  describe("Poller Status", () => {
    it("tracks poller count correctly", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      expect(poller.getPollerCount()).toBe(0);

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();
      expect(poller.getPollerCount()).toBe(1);

      poller.startPolling("terminal-2");
      await vi.runOnlyPendingTimersAsync();
      expect(poller.getPollerCount()).toBe(2);

      poller.stopPolling("terminal-1");
      expect(poller.getPollerCount()).toBe(1);

      poller.stopAll();
      expect(poller.getPollerCount()).toBe(0);
    });

    it("returns list of polled terminals", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      poller.startPolling("terminal-3");
      await vi.runOnlyPendingTimersAsync();

      const polled = poller.getPolledTerminals();
      expect(polled).toContain("terminal-1");
      expect(polled).toContain("terminal-3");
      expect(polled.length).toBe(2);
    });

    it("checks if specific terminal is being polled", async () => {
      vi.mocked(invoke).mockResolvedValue(createMockOutput("", 0));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(poller.isPolling("terminal-1")).toBe(true);
      expect(poller.isPolling("terminal-2")).toBe(false);
    });
  });

  describe("Singleton Instance", () => {
    it("returns same instance from getOutputPoller", () => {
      const instance1 = getOutputPoller();
      const instance2 = getOutputPoller();

      expect(instance1).toBe(instance2);
    });
  });

  describe("Real-world Scenarios", () => {
    it("handles continuous output stream from active terminal", async () => {
      const outputs = [
        createMockOutput("line 1\n", 7),
        createMockOutput("line 2\n", 14),
        createMockOutput("line 3\n", 21),
      ];

      let callIndex = 0;
      vi.mocked(invoke).mockImplementation(() => {
        if (callIndex < outputs.length) {
          return Promise.resolve(outputs[callIndex++]);
        }
        return Promise.resolve(createMockOutput("", outputs[outputs.length - 1].byte_count));
      });

      poller.startPolling("terminal-1");

      // First poll
      await vi.runOnlyPendingTimersAsync();
      expect(mockTerminalManager.clearAndWriteTerminal).toHaveBeenCalledWith(
        "terminal-1",
        "line 1\n"
      );

      mockTerminalManager.clearAndWriteTerminal.mockClear();

      // Second poll
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();
      expect(mockTerminalManager.writeToTerminal).toHaveBeenCalledWith("terminal-1", "line 2\n");

      // Third poll
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();
      expect(mockTerminalManager.writeToTerminal).toHaveBeenCalledWith("terminal-1", "line 3\n");
    });

    it("handles terminal that goes idle then resumes", async () => {
      const activityCallback = vi.fn();
      poller.onActivity(activityCallback);

      // Initial output
      vi.mocked(invoke)
        .mockResolvedValueOnce(createMockOutput("initial", 7))
        .mockResolvedValue(createMockOutput("", 7));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      expect(activityCallback).toHaveBeenCalledTimes(1);
      activityCallback.mockClear();

      // Wait for idle (30s+)
      vi.advanceTimersByTime(31000);
      await vi.runOnlyPendingTimersAsync();

      // Should have switched to idle polling
      assertLogContains(consoleLogSpy, "reducing poll frequency");

      // New output arrives
      vi.mocked(invoke).mockResolvedValueOnce(createMockOutput("resumed", 14));
      vi.advanceTimersByTime(10000);
      await vi.runOnlyPendingTimersAsync();

      expect(activityCallback).toHaveBeenCalledTimes(1);
      expect(mockTerminalManager.writeToTerminal).toHaveBeenCalledWith("terminal-1", "resumed");
    });

    it("handles terminal with intermittent IPC errors", async () => {
      const errorCallback = vi.fn();
      poller.onError(errorCallback);

      vi.mocked(invoke)
        .mockRejectedValueOnce(new Error("Error 1"))
        .mockResolvedValueOnce(createMockOutput("success", 7))
        .mockRejectedValueOnce(new Error("Error 2"))
        .mockResolvedValueOnce(createMockOutput("success again", 20));

      poller.startPolling("terminal-1");

      // Error then success
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      // Error counter should be reset
      let errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBe(0);

      // Another error then success
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBe(0);

      // Should not have called error callback (errors not consecutive)
      expect(errorCallback).not.toHaveBeenCalled();
    });

    it("manages multiple terminals with different activity patterns", async () => {
      const activityCallback = vi.fn();
      poller.onActivity(activityCallback);

      // Terminal 1: Active
      vi.mocked(invoke).mockImplementation((_cmd, args) => {
        const { id } = args as { id: string };
        if (id === "terminal-1") {
          return Promise.resolve(createMockOutput("active", 6));
        }
        return Promise.resolve(createMockOutput("", 0));
      });

      poller.startPolling("terminal-1");
      poller.startPolling("terminal-2");

      await vi.runOnlyPendingTimersAsync();

      // Terminal 1 should have activity callback
      expect(activityCallback).toHaveBeenCalledWith("terminal-1");
      expect(activityCallback).not.toHaveBeenCalledWith("terminal-2");

      // Both should be polling
      expect(poller.getPollerCount()).toBe(2);
    });

    it("recovers from fatal errors when restarted", async () => {
      poller.setMaxConsecutiveErrors(2);

      // Cause fatal errors
      vi.mocked(invoke).mockRejectedValue(new Error("Fatal"));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();
      vi.advanceTimersByTime(50);
      await vi.runOnlyPendingTimersAsync();

      // Should be stopped
      expect(poller.isPolling("terminal-1")).toBe(false);

      // Restart with working IPC
      vi.mocked(invoke).mockResolvedValue(createMockOutput("recovered", 9));

      poller.startPolling("terminal-1");
      await vi.runOnlyPendingTimersAsync();

      // Should be polling again with reset error count
      expect(poller.isPolling("terminal-1")).toBe(true);
      const errorState = poller.getErrorState("terminal-1");
      expect(errorState?.consecutiveErrors).toBe(0);
    });
  });
});
