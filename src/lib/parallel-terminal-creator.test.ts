import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  createTerminalsInParallel,
  createTerminalsWithRetry,
  retryFailedTerminals,
  type TerminalConfig,
} from "./parallel-terminal-creator";
import { AppState } from "./state";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

describe("parallel-terminal-creator", () => {
  let state: AppState;

  beforeEach(() => {
    vi.clearAllMocks();
    state = new AppState();
    state.setNextTerminalNumber(1);
  });

  describe("createTerminalsInParallel", () => {
    it("should create all terminals successfully in parallel", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 1,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
        {
          id: "terminal-3",
          name: "Agent 3",
          role: "reviewer",
          workingDir: "/workspace",
          instanceNumber: 3,
        },
      ];

      // Mock successful terminal creation
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };
          return `loom-${configId}`; // Return session ID
        }
        throw new Error("Unknown command");
      });

      const result = await createTerminalsInParallel(configs, "/workspace");

      expect(result.succeeded).toHaveLength(3);
      expect(result.failed).toHaveLength(0);
      expect(result.succeeded).toEqual([
        { configId: "terminal-1", terminalId: "loom-terminal-1" },
        { configId: "terminal-2", terminalId: "loom-terminal-2" },
        { configId: "terminal-3", terminalId: "loom-terminal-3" },
      ]);
    });

    it("should isolate failures and continue creating other terminals", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 1,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
        {
          id: "terminal-3",
          name: "Agent 3",
          role: "reviewer",
          workingDir: "/workspace",
          instanceNumber: 3,
        },
      ];

      // Mock terminal-2 failing, others succeeding
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };
          if (configId === "terminal-2") {
            throw new Error("Failed to create terminal-2");
          }
          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      const result = await createTerminalsInParallel(configs, "/workspace");

      expect(result.succeeded).toHaveLength(2);
      expect(result.failed).toHaveLength(1);
      expect(result.succeeded).toEqual([
        { configId: "terminal-1", terminalId: "loom-terminal-1" },
        { configId: "terminal-3", terminalId: "loom-terminal-3" },
      ]);
      expect(result.failed[0].configId).toBe("terminal-2");
    });

    it("should handle all terminals failing", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 1,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
      ];

      // Mock all terminals failing
      vi.mocked(invoke).mockRejectedValue(new Error("Daemon not running"));

      const result = await createTerminalsInParallel(configs, "/workspace");

      expect(result.succeeded).toHaveLength(0);
      expect(result.failed).toHaveLength(2);
    });

    it("should call invoke with correct parameters for each terminal", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Builder 1",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 1,
        },
      ];

      vi.mocked(invoke).mockResolvedValue("loom-terminal-1");

      await createTerminalsInParallel(configs, "/workspace");

      expect(invoke).toHaveBeenCalledWith("create_terminal", {
        configId: "terminal-1",
        name: "Builder 1",
        workingDir: "/workspace",
        role: "worker",
        instanceNumber: 1,
      });
    });
  });

  describe("retryFailedTerminals", () => {
    it("should retry failed terminals with exponential backoff", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
      ];

      const failed = [
        {
          configId: "terminal-2",
          error: new Error("Initial failure"),
        },
      ];

      // Mock succeeding on first retry
      vi.mocked(invoke).mockResolvedValue("loom-terminal-2");

      const result = await retryFailedTerminals(failed, configs, "/workspace", 2);

      expect(result.succeeded).toHaveLength(1);
      expect(result.failed).toHaveLength(0);
      expect(result.succeeded[0]).toEqual({
        configId: "terminal-2",
        terminalId: "loom-terminal-2",
      });

      // Should have been called once for the retry
      expect(invoke).toHaveBeenCalledTimes(1);
    });

    it("should exhaust retries and report final failures", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
      ];

      const failed = [
        {
          configId: "terminal-2",
          error: new Error("Initial failure"),
        },
      ];

      // Mock all retries failing
      vi.mocked(invoke).mockRejectedValue(new Error("Persistent failure"));

      const result = await retryFailedTerminals(failed, configs, "/workspace", 2);

      expect(result.succeeded).toHaveLength(0);
      expect(result.failed).toHaveLength(1);
      expect(result.failed[0].configId).toBe("terminal-2");

      // Should have been called twice (2 retries)
      expect(invoke).toHaveBeenCalledTimes(2);
    });

    it("should retry multiple failed terminals independently", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 2,
        },
        {
          id: "terminal-4",
          name: "Agent 4",
          role: "reviewer",
          workingDir: "/workspace",
          instanceNumber: 4,
        },
      ];

      const failed = [
        { configId: "terminal-2", error: new Error("Failure 1") },
        { configId: "terminal-4", error: new Error("Failure 2") },
      ];

      // Mock terminal-2 succeeding on retry, terminal-4 failing
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };
          if (configId === "terminal-2") {
            return "loom-terminal-2";
          }
          throw new Error("Still failing");
        }
        throw new Error("Unknown command");
      });

      const result = await retryFailedTerminals(failed, configs, "/workspace", 2);

      expect(result.succeeded).toHaveLength(1);
      expect(result.failed).toHaveLength(1);
      expect(result.succeeded[0].configId).toBe("terminal-2");
      expect(result.failed[0].configId).toBe("terminal-4");
    });

    it("should handle missing config gracefully", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 1,
        },
      ];

      const failed = [
        {
          configId: "terminal-999", // Config doesn't exist
          error: new Error("Unknown"),
        },
      ];

      const result = await retryFailedTerminals(failed, configs, "/workspace", 2);

      expect(result.succeeded).toHaveLength(0);
      expect(result.failed).toHaveLength(1);
      expect(result.failed[0].configId).toBe("terminal-999");

      // Should not call invoke since config wasn't found
      expect(invoke).not.toHaveBeenCalled();
    });
  });

  describe("createTerminalsWithRetry", () => {
    it("should create all terminals successfully on first attempt", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 0, // Will be assigned
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 0, // Will be assigned
        },
      ];

      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };
          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      const result = await createTerminalsWithRetry(configs, "/workspace", state);

      expect(result.succeeded).toHaveLength(2);
      expect(result.failed).toHaveLength(0);
      expect(state.getCurrentTerminalNumber()).toBe(3); // Should have incremented to 3
    });

    it("should retry failed terminals automatically", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
      ];

      let callCount = 0;
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };
          callCount++;

          // First call for terminal-2 fails, retry succeeds
          if (configId === "terminal-2" && callCount === 2) {
            throw new Error("Transient failure");
          }

          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      const result = await createTerminalsWithRetry(configs, "/workspace", state);

      expect(result.succeeded).toHaveLength(2);
      expect(result.failed).toHaveLength(0);

      // Should have called invoke 3 times: 2 initial + 1 retry
      expect(invoke).toHaveBeenCalledTimes(3);
    });

    it("should report persistent failures after retries", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
      ];

      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          const { configId } = args as { configId: string };

          // terminal-2 always fails
          if (configId === "terminal-2") {
            throw new Error("Persistent failure");
          }

          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      const result = await createTerminalsWithRetry(configs, "/workspace", state);

      expect(result.succeeded).toHaveLength(1);
      expect(result.failed).toHaveLength(1);
      expect(result.succeeded[0].configId).toBe("terminal-1");
      expect(result.failed[0].configId).toBe("terminal-2");
    });

    it("should assign unique instance numbers to each terminal", async () => {
      const configs: TerminalConfig[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          role: "default",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
        {
          id: "terminal-3",
          name: "Agent 3",
          role: "reviewer",
          workingDir: "/workspace",
          instanceNumber: 0,
        },
      ];

      const instanceNumbers: number[] = [];
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (
          cmd === "create_terminal" &&
          args &&
          typeof args === "object" &&
          "instanceNumber" in args
        ) {
          const { instanceNumber, configId } = args as { instanceNumber: number; configId: string };
          instanceNumbers.push(instanceNumber);
          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      await createTerminalsWithRetry(configs, "/workspace", state);

      // Should have assigned 1, 2, 3
      expect(instanceNumbers).toEqual([1, 2, 3]);
      expect(state.getCurrentTerminalNumber()).toBe(4);
    });

    it("should handle empty config array", async () => {
      const configs: TerminalConfig[] = [];

      const result = await createTerminalsWithRetry(configs, "/workspace", state);

      expect(result.succeeded).toHaveLength(0);
      expect(result.failed).toHaveLength(0);
      expect(invoke).not.toHaveBeenCalled();
    });
  });

  describe("performance", () => {
    it("should create terminals faster than sequential creation", async () => {
      const configs: TerminalConfig[] = Array.from({ length: 7 }, (_, i) => ({
        id: `terminal-${i + 1}`,
        name: `Agent ${i + 1}`,
        role: "default",
        workingDir: "/workspace",
        instanceNumber: i + 1,
      }));

      // Mock each terminal creation taking 50ms
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "create_terminal" && args && typeof args === "object" && "configId" in args) {
          await new Promise((resolve) => setTimeout(resolve, 50));
          const { configId } = args as { configId: string };
          return `loom-${configId}`;
        }
        throw new Error("Unknown command");
      });

      const startTime = Date.now();
      const result = await createTerminalsInParallel(configs, "/workspace");
      const elapsedMs = Date.now() - startTime;

      expect(result.succeeded).toHaveLength(7);
      expect(result.failed).toHaveLength(0);

      // Parallel should take ~50ms (all at once)
      // Sequential would take ~350ms (7 × 50ms)
      // Allow some margin for test execution overhead
      expect(elapsedMs).toBeLessThan(150); // Should be much faster than 350ms
    });
  });
});
