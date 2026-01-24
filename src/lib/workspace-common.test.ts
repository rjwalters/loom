import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AppState, Terminal } from "./state";
import { initializeTerminals } from "./workspace-common";

// Mock dependencies
vi.mock("./config", () => ({
  saveCurrentConfiguration: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("./parallel-terminal-creator", () => ({
  createTerminalsWithRetry: vi.fn().mockResolvedValue({
    succeeded: [
      { configId: "terminal-1", terminalId: "session-1" },
      { configId: "terminal-2", terminalId: "session-2" },
    ],
    failed: [],
  }),
}));

vi.mock("./toast", () => ({
  showToast: vi.fn(),
}));

vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      error: vi.fn(),
      warn: vi.fn(),
    }),
  },
}));

describe("workspace-common", () => {
  let mockState: AppState;
  let mockLaunchAgentsForTerminals: (workspacePath: string, terminals: Terminal[]) => Promise<void>;
  let mockAgents: Terminal[];

  beforeEach(() => {
    // Reset mocks
    vi.clearAllMocks();

    // Create mock agents
    mockAgents = [
      {
        id: "terminal-1",
        name: "Builder",
        status: "idle" as unknown as Terminal["status"],
        isPrimary: true,
        role: "builder",
      },
      {
        id: "terminal-2",
        name: "Judge",
        status: "idle" as unknown as Terminal["status"],
        isPrimary: false,
        role: "judge",
      },
    ];

    // Create mock state
    mockState = {
      workspace: {
        setWorkspace: vi.fn(),
      },
      terminals: {
        loadTerminals: vi.fn(),
        getTerminals: vi.fn().mockReturnValue(mockAgents),
        getNextTerminalNumber: vi.fn().mockReturnValue(1),
      },
    } as unknown as AppState;

    // Create mock launcher
    mockLaunchAgentsForTerminals = vi.fn() as unknown as (
      workspacePath: string,
      terminals: Terminal[]
    ) => Promise<void>;
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("initializeTerminals", () => {
    it("should create terminals and update agent IDs", async () => {
      const result = await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
      });

      // Check result counts
      expect(result.succeededCount).toBe(2);
      expect(result.failedCount).toBe(0);

      // Check that agent IDs were updated
      expect(mockAgents[0].id).toBe("session-1");
      expect(mockAgents[1].id).toBe("session-2");
    });

    it("should load terminals into state", async () => {
      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
      });

      expect(mockState.terminals.loadTerminals).toHaveBeenCalledWith(mockAgents);
    });

    it("should launch agents for terminals", async () => {
      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
      });

      expect(mockLaunchAgentsForTerminals).toHaveBeenCalledWith("/test/workspace", mockAgents);
    });

    it("should save configuration after launch", async () => {
      const { saveCurrentConfiguration } = await import("./config");

      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
      });

      expect(saveCurrentConfiguration).toHaveBeenCalled();
    });

    it("should clear worktree paths when clearWorktreePaths is true", async () => {
      mockAgents[0].worktreePath = "/some/path";
      mockAgents[1].worktreePath = "/another/path";

      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        clearWorktreePaths: true,
      });

      expect(mockAgents[0].worktreePath).toBe("");
      expect(mockAgents[1].worktreePath).toBe("");
    });

    it("should set workspace active when setWorkspaceActive is true", async () => {
      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        setWorkspaceActive: true,
      });

      expect(mockState.workspace.setWorkspace).toHaveBeenCalledWith("/test/workspace");
    });

    it("should not set workspace active when setWorkspaceActive is false", async () => {
      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        setWorkspaceActive: false,
      });

      expect(mockState.workspace.setWorkspace).not.toHaveBeenCalled();
    });

    it("should save config before launch when saveBeforeLaunch is true", async () => {
      const { saveCurrentConfiguration } = await import("./config");
      const saveMock = saveCurrentConfiguration as ReturnType<typeof vi.fn>;

      await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        saveBeforeLaunch: true,
      });

      // With saveBeforeLaunch=true, save should be called twice (before and after launch)
      expect(saveMock).toHaveBeenCalledTimes(2);
    });

    it("should handle failures and show toast per failure when toastPerFailure is true", async () => {
      const { createTerminalsWithRetry } = await import("./parallel-terminal-creator");
      const createMock = createTerminalsWithRetry as ReturnType<typeof vi.fn>;
      createMock.mockResolvedValueOnce({
        succeeded: [{ configId: "terminal-1", terminalId: "session-1" }],
        failed: [{ configId: "terminal-2", error: new Error("Failed to create") }],
      });

      const { showToast } = await import("./toast");

      const result = await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        toastPerFailure: true,
      });

      expect(result.succeededCount).toBe(1);
      expect(result.failedCount).toBe(1);
      expect(showToast).toHaveBeenCalledTimes(1);
      expect(showToast).toHaveBeenCalledWith(expect.stringContaining("Judge"), "error");
    });

    it("should handle failures and show combined toast when toastPerFailure is false", async () => {
      const { createTerminalsWithRetry } = await import("./parallel-terminal-creator");
      const createMock = createTerminalsWithRetry as ReturnType<typeof vi.fn>;
      createMock.mockResolvedValueOnce({
        succeeded: [{ configId: "terminal-1", terminalId: "session-1" }],
        failed: [{ configId: "terminal-2", error: new Error("Failed to create") }],
      });

      const { showToast } = await import("./toast");

      const result = await initializeTerminals({
        workspacePath: "/test/workspace",
        agents: mockAgents,
        state: mockState,
        launchAgentsForTerminals: mockLaunchAgentsForTerminals,
        toastPerFailure: false,
      });

      expect(result.succeededCount).toBe(1);
      expect(result.failedCount).toBe(1);
      expect(showToast).toHaveBeenCalledTimes(1);
      expect(showToast).toHaveBeenCalledWith(
        expect.stringContaining("Failed to create 1 terminal"),
        "error",
        7000
      );
    });
  });
});
