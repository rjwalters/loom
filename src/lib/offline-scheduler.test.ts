import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startOfflineScheduler, stopOfflineScheduler } from "./offline-scheduler";
import type { Terminal } from "./state";
import { TerminalStatus } from "./state";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock Logger
vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      error: vi.fn(),
      warn: vi.fn(),
    }),
  },
}));

describe("offline-scheduler", () => {
  let invokeMock: ReturnType<typeof vi.fn>;

  // Sample terminal data
  const mockTerminals: Terminal[] = [
    {
      id: "terminal-1",
      name: "Builder",
      status: TerminalStatus.Idle,
      isPrimary: true,
    },
    {
      id: "terminal-2",
      name: "Judge",
      status: TerminalStatus.Idle,
      isPrimary: false,
    },
  ];

  const mockWorkspacePath = "/test/workspace";

  beforeEach(async () => {
    vi.useFakeTimers();
    const { invoke } = await import("@tauri-apps/api/tauri");
    invokeMock = vi.mocked(invoke);
    invokeMock.mockClear();
    invokeMock.mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
    stopOfflineScheduler();
  });

  describe("startOfflineScheduler", () => {
    it("sends initial status echo to all terminals immediately", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      // Wait for all async operations to complete (2 terminals x 2 calls each = 4 total)
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      // Should send echo command to each terminal
      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: expect.stringContaining("Status echo - still alive"),
      });

      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-2",
        data: expect.stringContaining("Status echo - still alive"),
      });

      // Should send Enter key to execute the command
      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });

      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-2",
        data: "\r",
      });
    });

    it("includes ANSI color codes in status echo", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      // Wait for the async sendOfflineStatusEchoes to complete
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalled();
      });

      // Check for green color code (\033[32m) and cyan color code (\033[36m)
      const calls = invokeMock.mock.calls.filter(
        (call) =>
          call[0] === "send_terminal_input" &&
          typeof call[1] === "object" &&
          "data" in call[1] &&
          typeof call[1].data === "string" &&
          call[1].data.includes("echo")
      );

      expect(calls.length).toBeGreaterThan(0);
      const echoCall = calls[0];
      expect(echoCall[1].data).toContain("\\033[32m"); // Green
      expect(echoCall[1].data).toContain("\\033[36m"); // Cyan
      expect(echoCall[1].data).toContain("\\033[0m"); // Reset
    });

    it("includes timestamp in status echo", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      // Wait for the async sendOfflineStatusEchoes to complete
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalled();
      });

      const calls = invokeMock.mock.calls.filter(
        (call) =>
          call[0] === "send_terminal_input" &&
          typeof call[1] === "object" &&
          "data" in call[1] &&
          typeof call[1].data === "string" &&
          call[1].data.includes("echo")
      );

      expect(calls.length).toBeGreaterThan(0);
      const echoCall = calls[0];
      const echoData = echoCall[1].data as string;

      // Extract timestamp from the echo command
      // Should be in ISO format
      expect(echoData).toMatch(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z/);
    });

    it("schedules periodic status echoes at default interval (30s)", async () => {
      invokeMock.mockClear();
      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      // Initial calls should be made
      const initialCallCount = invokeMock.mock.calls.length;
      expect(initialCallCount).toBeGreaterThan(0);

      invokeMock.mockClear();

      // Advance time by 30 seconds
      await vi.advanceTimersByTimeAsync(30000);

      // Should have sent status echoes again
      expect(invokeMock).toHaveBeenCalled();
      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: expect.stringContaining("Status echo"),
      });
    });

    it("schedules periodic status echoes at custom interval", async () => {
      const customInterval = 5000; // 5 seconds
      invokeMock.mockClear();
      startOfflineScheduler(mockTerminals, mockWorkspacePath, customInterval);

      // Initial calls should be made
      expect(invokeMock).toHaveBeenCalled();

      invokeMock.mockClear();

      // Advance time by custom interval
      await vi.advanceTimersByTimeAsync(customInterval);

      // Should have sent status echoes again
      expect(invokeMock).toHaveBeenCalled();
    });

    it("stops existing scheduler before starting new one", async () => {
      // Start first scheduler
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      // Wait for initial calls to complete (2 terminals x 2 calls)
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      invokeMock.mockClear();

      // Start second scheduler (should stop first)
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 2000);

      // Wait for second scheduler's initial calls
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      // Clear calls from second scheduler start
      invokeMock.mockClear();

      // Advance by 1 second (old interval) - should not trigger
      await vi.advanceTimersByTimeAsync(1000);
      expect(invokeMock).not.toHaveBeenCalled();

      // Advance by 1 more second (total 2 seconds - new interval) - should trigger
      await vi.advanceTimersByTimeAsync(1000);
      expect(invokeMock).toHaveBeenCalled();

      // Cleanup
      stopOfflineScheduler();
    }, 10000);

    it("handles errors when sending status echo", async () => {
      invokeMock.mockRejectedValueOnce(new Error("Terminal not found"));

      // Should not throw error
      expect(() => {
        startOfflineScheduler(mockTerminals, mockWorkspacePath);
      }).not.toThrow();

      // Wait for initial async call to settle
      await vi.waitFor(() => {
        // Check that invoke was called despite the error
        expect(invokeMock).toHaveBeenCalled();
      });

      // Stop scheduler to prevent infinite loop
      stopOfflineScheduler();
    });

    it("continues sending echoes to other terminals if one fails", async () => {
      // Fail on first terminal, succeed on second
      invokeMock.mockRejectedValueOnce(new Error("Terminal 1 failed")).mockResolvedValue(undefined);

      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      // Wait for initial async calls to settle
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalled();
      });

      // Stop scheduler to prevent infinite loop
      stopOfflineScheduler();

      // Should have attempted to send to both terminals
      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: expect.stringContaining("Status echo"),
      });

      expect(invokeMock).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-2",
        data: expect.stringContaining("Status echo"),
      });
    });

    it("works with empty terminal array", () => {
      expect(() => {
        startOfflineScheduler([], mockWorkspacePath);
      }).not.toThrow();

      // Should not invoke any terminal commands
      const terminalCalls = invokeMock.mock.calls.filter(
        (call) => call[0] === "send_terminal_input"
      );
      expect(terminalCalls).toHaveLength(0);
    });
  });

  describe("stopOfflineScheduler", () => {
    it("stops the scheduler interval", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      // Wait for initial calls to complete
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      invokeMock.mockClear();

      stopOfflineScheduler();

      // Advance time past interval
      await vi.advanceTimersByTimeAsync(5000);

      // Should not have sent any more status echoes
      expect(invokeMock).not.toHaveBeenCalled();
    }, 10000);

    it("can be called multiple times safely", () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath);

      expect(() => {
        stopOfflineScheduler();
        stopOfflineScheduler();
        stopOfflineScheduler();
      }).not.toThrow();
    });

    it("can be called before starting scheduler", () => {
      expect(() => {
        stopOfflineScheduler();
      }).not.toThrow();
    });

    it("prevents previously scheduled echoes from executing", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      // Advance time halfway to interval
      await vi.advanceTimersByTimeAsync(500);

      invokeMock.mockClear();
      stopOfflineScheduler();

      // Advance past the original interval time
      await vi.advanceTimersByTimeAsync(1000);

      // No echoes should have been sent
      expect(invokeMock).not.toHaveBeenCalled();
    });
  });

  describe("integration scenarios", () => {
    it("handles start -> stop -> start cycle correctly", async () => {
      // First start
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      // Wait for initial calls to complete
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      invokeMock.mockClear();

      // Stop
      stopOfflineScheduler();

      // Verify stopped
      await vi.advanceTimersByTimeAsync(2000);
      expect(invokeMock).not.toHaveBeenCalled();

      // Start again
      invokeMock.mockClear();
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      // Wait for initial echoes
      await vi.waitFor(() => {
        expect(invokeMock).toHaveBeenCalledTimes(4);
      });

      invokeMock.mockClear();

      // Should schedule new interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(invokeMock).toHaveBeenCalled();

      // Cleanup
      stopOfflineScheduler();
    }, 10000);

    it("maintains separate timestamps for each status echo batch", async () => {
      startOfflineScheduler(mockTerminals, mockWorkspacePath, 1000);

      const getLastEchoTimestamp = () => {
        const calls = invokeMock.mock.calls.filter(
          (call) =>
            call[0] === "send_terminal_input" &&
            typeof call[1] === "object" &&
            "data" in call[1] &&
            typeof call[1].data === "string" &&
            call[1].data.includes("echo")
        );
        if (calls.length === 0) return null;
        const lastCall = calls[calls.length - 1];
        const match = (lastCall[1].data as string).match(
          /(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z)/
        );
        return match ? match[1] : null;
      };

      const firstTimestamp = getLastEchoTimestamp();
      expect(firstTimestamp).not.toBeNull();

      invokeMock.mockClear();

      // Advance time and get second batch
      await vi.advanceTimersByTimeAsync(1000);

      const secondTimestamp = getLastEchoTimestamp();
      expect(secondTimestamp).not.toBeNull();

      // Timestamps should be different (second batch should be later)
      expect(secondTimestamp).not.toBe(firstTimestamp);
    });
  });
});
