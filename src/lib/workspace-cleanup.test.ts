import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { WorkspaceCleanupOptions } from "./workspace-cleanup";

// Mock Tauri API
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// Mock Logger
vi.mock("./logger", () => ({
  Logger: {
    forComponent: vi.fn(() => ({
      info: vi.fn(),
      error: vi.fn(),
    })),
  },
}));

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import { cleanupWorkspace } from "./workspace-cleanup";

describe("cleanupWorkspace", () => {
  let mockState: any;
  let mockOutputPoller: any;
  let mockTerminalManager: any;
  let mockSetCurrentAttachedTerminalId: any;
  let mockLogger: any;
  let mockTerminals: any[];

  beforeEach(() => {
    vi.clearAllMocks();

    // Create mock terminals
    mockTerminals = [
      { id: "terminal-1", name: "Worker 1" },
      { id: "terminal-2", name: "Reviewer 1" },
      { id: "terminal-3", name: "Architect 1" },
    ];

    // Mock state
    mockState = {
      terminals: {
        getTerminals: vi.fn(() => mockTerminals),
      },
      clearAll: vi.fn(),
    };

    // Mock output poller
    mockOutputPoller = {
      stopPolling: vi.fn(),
    };

    // Mock terminal manager
    mockTerminalManager = {
      destroyAll: vi.fn(),
    };

    // Mock callback
    mockSetCurrentAttachedTerminalId = vi.fn();

    // Mock logger
    mockLogger = {
      info: vi.fn(),
      error: vi.fn(),
    };

    vi.mocked(Logger.forComponent).mockReturnValue(mockLogger);
    vi.mocked(invoke).mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  describe("Cleanup Sequence", () => {
    it("executes cleanup steps in correct order", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Verify cleanup sequence
      expect(mockState.terminals.getTerminals).toHaveBeenCalled();
      expect(mockOutputPoller.stopPolling).toHaveBeenCalledTimes(3);
      expect(mockTerminalManager.destroyAll).toHaveBeenCalled();
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-1" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-2" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-3" });
      expect(invoke).toHaveBeenCalledWith("kill_all_loom_sessions");
      expect(mockState.clearAll).toHaveBeenCalled();
      expect(mockSetCurrentAttachedTerminalId).toHaveBeenCalledWith(null);
    });

    it("stops polling for each terminal", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockOutputPoller.stopPolling).toHaveBeenCalledWith("terminal-1");
      expect(mockOutputPoller.stopPolling).toHaveBeenCalledWith("terminal-2");
      expect(mockOutputPoller.stopPolling).toHaveBeenCalledWith("terminal-3");
    });

    it("destroys all xterm instances", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockTerminalManager.destroyAll).toHaveBeenCalledTimes(1);
    });

    it("destroys all terminal sessions", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-1" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-2" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-3" });
    });

    it("kills all loom tmux sessions", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(invoke).toHaveBeenCalledWith("kill_all_loom_sessions");
    });

    it("clears state and resets attached terminal ID", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockState.clearAll).toHaveBeenCalled();
      expect(mockSetCurrentAttachedTerminalId).toHaveBeenCalledWith(null);
    });
  });

  describe("Error Handling", () => {
    it("continues cleanup when destroy_terminal fails for one terminal", async () => {
      vi.mocked(invoke).mockImplementation((cmd: string, args?: any) => {
        if (cmd === "destroy_terminal" && args?.id === "terminal-2") {
          return Promise.reject(new Error("Failed to destroy terminal"));
        }
        return Promise.resolve(undefined);
      });

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Should still attempt to destroy all terminals
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-1" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-2" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-3" });

      // Should continue with cleanup
      expect(invoke).toHaveBeenCalledWith("kill_all_loom_sessions");
      expect(mockState.clearAll).toHaveBeenCalled();

      // Should log error
      expect(mockLogger.error).toHaveBeenCalledWith(
        "Failed to destroy terminal",
        expect.any(Error),
        { terminalId: "terminal-2" }
      );
    });

    it("continues cleanup when kill_all_loom_sessions fails", async () => {
      vi.mocked(invoke).mockImplementation((cmd: string) => {
        if (cmd === "kill_all_loom_sessions") {
          return Promise.reject(new Error("Failed to kill sessions"));
        }
        return Promise.resolve(undefined);
      });

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Should still complete state cleanup
      expect(mockState.clearAll).toHaveBeenCalled();
      expect(mockSetCurrentAttachedTerminalId).toHaveBeenCalledWith(null);

      // Should log error
      expect(mockLogger.error).toHaveBeenCalledWith(
        "Failed to kill loom sessions",
        expect.any(Error)
      );
    });

    it("destroys all terminals even when some fail", async () => {
      let callCount = 0;
      vi.mocked(invoke).mockImplementation((cmd: string) => {
        if (cmd === "destroy_terminal") {
          callCount++;
          if (callCount % 2 === 0) {
            return Promise.reject(new Error("Simulated failure"));
          }
        }
        return Promise.resolve(undefined);
      });

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Should attempt all 3 destroys
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-1" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-2" });
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-3" });
    });
  });

  describe("Logging", () => {
    it("creates logger with correct component name", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "workspace-start",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(Logger.forComponent).toHaveBeenCalledWith("workspace-start");
    });

    it("logs cleanup progress with structured logging", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Check structured logging calls
      expect(mockLogger.info).toHaveBeenCalledWith("Stopping output polling for all terminals", {
        terminalCount: 3,
      });
      expect(mockLogger.info).toHaveBeenCalledWith("Destroying xterm instances");
      expect(mockLogger.info).toHaveBeenCalledWith("Destroying terminal sessions", {
        terminalCount: 3,
      });
      expect(mockLogger.info).toHaveBeenCalledWith("Killing all loom tmux sessions");
      expect(mockLogger.info).toHaveBeenCalledWith("Clearing terminals from state");
      expect(mockLogger.info).toHaveBeenCalledWith("Workspace cleanup complete");
    });

    it("logs successful terminal destruction", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockLogger.info).toHaveBeenCalledWith("Destroyed terminal session", {
        terminalId: "terminal-1",
        terminalName: "Worker 1",
      });
      expect(mockLogger.info).toHaveBeenCalledWith("Destroyed terminal session", {
        terminalId: "terminal-2",
        terminalName: "Reviewer 1",
      });
      expect(mockLogger.info).toHaveBeenCalledWith("Destroyed terminal session", {
        terminalId: "terminal-3",
        terminalName: "Architect 1",
      });
    });

    it("logs errors with proper context", async () => {
      const testError = new Error("Destroy failed");
      vi.mocked(invoke).mockImplementation((cmd: string, args?: any) => {
        if (cmd === "destroy_terminal" && args?.id === "terminal-2") {
          return Promise.reject(testError);
        }
        return Promise.resolve(undefined);
      });

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockLogger.error).toHaveBeenCalledWith("Failed to destroy terminal", testError, {
        terminalId: "terminal-2",
      });
    });
  });

  describe("Edge Cases", () => {
    it("handles empty terminal list", async () => {
      mockState.terminals.getTerminals.mockReturnValue([]);

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      // Should still complete all steps
      expect(mockOutputPoller.stopPolling).not.toHaveBeenCalled();
      expect(mockTerminalManager.destroyAll).toHaveBeenCalled();
      expect(invoke).toHaveBeenCalledWith("kill_all_loom_sessions");
      expect(mockState.clearAll).toHaveBeenCalled();
      expect(mockLogger.info).toHaveBeenCalledWith("Stopping output polling for all terminals", {
        terminalCount: 0,
      });
    });

    it("handles single terminal", async () => {
      mockState.terminals.getTerminals.mockReturnValue([{ id: "terminal-1", name: "Solo" }]);

      const options: WorkspaceCleanupOptions = {
        component: "test-component",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(mockOutputPoller.stopPolling).toHaveBeenCalledTimes(1);
      expect(mockOutputPoller.stopPolling).toHaveBeenCalledWith("terminal-1");
      expect(invoke).toHaveBeenCalledWith("destroy_terminal", { id: "terminal-1" });
    });
  });

  describe("Integration with Different Components", () => {
    it("works with workspace-start component", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "start-workspace",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(Logger.forComponent).toHaveBeenCalledWith("start-workspace");
      expect(mockState.clearAll).toHaveBeenCalled();
    });

    it("works with workspace-reset component", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "workspace-reset",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(Logger.forComponent).toHaveBeenCalledWith("workspace-reset");
      expect(mockState.clearAll).toHaveBeenCalled();
    });

    it("works with force-start-workspace component", async () => {
      const options: WorkspaceCleanupOptions = {
        component: "force-start-workspace",
        state: mockState,
        outputPoller: mockOutputPoller,
        terminalManager: mockTerminalManager,
        setCurrentAttachedTerminalId: mockSetCurrentAttachedTerminalId,
      };

      await cleanupWorkspace(options);

      expect(Logger.forComponent).toHaveBeenCalledWith("force-start-workspace");
      expect(mockState.clearAll).toHaveBeenCalled();
    });
  });
});
