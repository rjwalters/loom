import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppState, TerminalStatus } from "./state";

// Mock dependencies
vi.mock("./logger", () => ({
  Logger: {
    forComponent: vi.fn(() => ({
      info: vi.fn(),
      warn: vi.fn(),
      error: vi.fn(),
    })),
  },
}));

vi.mock("./agent-launcher", () => ({
  sendPromptToAgent: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("./terminal-state-parser", () => ({
  detectTerminalState: vi.fn().mockResolvedValue({
    status: "idle",
    isWaiting: true,
    hasError: false,
  }),
}));

describe("interval-prompt-manager", () => {
  let state: AppState;

  beforeEach(() => {
    vi.useFakeTimers();
    state = new AppState();

    // Clear the singleton instance
    vi.resetModules();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  describe("getIntervalPromptManager", () => {
    it("returns singleton instance", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager1 = getIntervalPromptManager();
      const manager2 = getIntervalPromptManager();

      expect(manager1).toBe(manager2);
    });
  });

  describe("start/stop", () => {
    it("starts management for terminal with valid interval", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Continue working",
        },
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      manager.start(terminal);

      expect(manager.isManaged("term-1")).toBe(true);
    });

    it("stops management for terminal", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Continue working",
        },
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      manager.start(terminal);
      expect(manager.isManaged("term-1")).toBe(true);

      manager.stop("term-1");
      expect(manager.isManaged("term-1")).toBe(false);
    });

    it("does not start management for terminal without interval", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: -1, // Invalid interval
          intervalPrompt: "Continue working",
        },
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      manager.start(terminal);

      expect(manager.isManaged("term-1")).toBe(false);
    });
  });

  describe("startAll/stopAll", () => {
    it("starts management for all eligible terminals", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      // Add multiple terminals
      state.terminals.addTerminal({
        id: "term-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Work on issues",
        },
      });

      state.terminals.addTerminal({
        id: "term-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 120000,
          intervalPrompt: "Review PRs",
        },
      });

      state.terminals.addTerminal({
        id: "term-3",
        name: "Manual Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        // No roleConfig - should not be managed
      });

      manager.startAll(state);

      expect(manager.isManaged("term-1")).toBe(true);
      expect(manager.isManaged("term-2")).toBe(true);
      expect(manager.isManaged("term-3")).toBe(false);
    });

    it("stops management for all terminals", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      // Add and start terminals
      state.terminals.addTerminal({
        id: "term-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Work on issues",
        },
      });

      state.terminals.addTerminal({
        id: "term-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 120000,
          intervalPrompt: "Review PRs",
        },
      });

      manager.startAll(state);
      expect(manager.isManaged("term-1")).toBe(true);
      expect(manager.isManaged("term-2")).toBe(true);

      manager.stopAll();
      expect(manager.isManaged("term-1")).toBe(false);
      expect(manager.isManaged("term-2")).toBe(false);
    });
  });

  describe("runNow", () => {
    it("manually triggers prompt for managed terminal", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const { sendPromptToAgent } = await import("./agent-launcher");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Custom prompt",
        },
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      manager.start(terminal);

      await manager.runNow(terminal);

      expect(sendPromptToAgent).toHaveBeenCalledWith("term-1", "Custom prompt");
    });

    it("does nothing for unmanaged terminal", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const { sendPromptToAgent } = await import("./agent-launcher");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      // Don't start management

      await manager.runNow(terminal);

      expect(sendPromptToAgent).not.toHaveBeenCalled();
    });
  });

  describe("getStatus/getAllStatus", () => {
    it("returns status for managed terminal", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Work prompt",
        },
      });

      const terminal = state.terminals.getTerminal("term-1")!;
      manager.start(terminal);

      const status = manager.getStatus("term-1");
      expect(status).toBeDefined();
      expect(status?.terminalId).toBe("term-1");
      expect(status?.minInterval).toBe(60000);
      expect(status?.intervalPrompt).toBe("Work prompt");
    });

    it("returns undefined for unmanaged terminal", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      const status = manager.getStatus("nonexistent");
      expect(status).toBeUndefined();
    });

    it("returns all managed terminal statuses", async () => {
      const { getIntervalPromptManager } = await import("./interval-prompt-manager");
      const manager = getIntervalPromptManager();

      state.terminals.addTerminal({
        id: "term-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 60000,
          intervalPrompt: "Prompt 1",
        },
      });

      state.terminals.addTerminal({
        id: "term-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
        roleConfig: {
          targetInterval: 120000,
          intervalPrompt: "Prompt 2",
        },
      });

      manager.startAll(state);

      const allStatus = manager.getAllStatus();
      expect(allStatus).toHaveLength(2);
      expect(allStatus.map((s) => s.terminalId).sort()).toEqual(["term-1", "term-2"]);
    });
  });
});
