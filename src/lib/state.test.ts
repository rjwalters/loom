import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentStatus, AppState, TerminalStatus } from "./state";

describe("AppState", () => {
  let state: AppState;

  beforeEach(() => {
    state = new AppState();
  });

  describe("Terminal Management", () => {
    it("should add a terminal", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Test Terminal",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const terminals = state.getTerminals();
      expect(terminals).toHaveLength(1);
      expect(terminals[0].name).toBe("Test Terminal");
    });

    it("should set terminal as primary when isPrimary is true", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: true,
      });

      const primary = state.getPrimary();
      expect(primary).not.toBeNull();
      expect(primary?.id).toBe("terminal-1");
    });

    it("should remove a terminal", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.removeTerminal("terminal-1");
      expect(state.getTerminals()).toHaveLength(0);
    });

    it("should promote first terminal to primary when primary is removed", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: true,
      });
      state.addTerminal({
        id: "terminal-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.removeTerminal("terminal-1");

      const primary = state.getPrimary();
      expect(primary?.id).toBe("terminal-2");
      expect(primary?.isPrimary).toBe(true);
    });

    it("should set primary terminal", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: true,
      });
      state.addTerminal({
        id: "terminal-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.setPrimary("terminal-2");

      const terminals = state.getTerminals();
      expect(terminals.find((t) => t.id === "terminal-1")?.isPrimary).toBe(false);
      expect(terminals.find((t) => t.id === "terminal-2")?.isPrimary).toBe(true);
    });

    it("should rename a terminal", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Old Name",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.renameTerminal("terminal-1", "New Name");

      const terminal = state.getTerminals()[0];
      expect(terminal.name).toBe("New Name");
    });

    it("should update terminal properties", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.updateTerminal("terminal-1", {
        status: TerminalStatus.Busy,
        agentStatus: AgentStatus.Ready,
      });

      const terminal = state.getTerminals()[0];
      expect(terminal.status).toBe(TerminalStatus.Busy);
      expect(terminal.agentStatus).toBe(AgentStatus.Ready);
    });
  });

  describe("Terminal Ordering", () => {
    it("should maintain terminal order", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "First",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });
      state.addTerminal({
        id: "terminal-2",
        name: "Second",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });
      state.addTerminal({
        id: "terminal-3",
        name: "Third",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const terminals = state.getTerminals();
      expect(terminals.map((t) => t.name)).toEqual(["First", "Second", "Third"]);
    });

    it("should reorder terminals", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "First",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });
      state.addTerminal({
        id: "terminal-2",
        name: "Second",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });
      state.addTerminal({
        id: "terminal-3",
        name: "Third",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      // Move "First" to after "Third"
      state.reorderTerminal("terminal-1", "terminal-3", false);

      const terminals = state.getTerminals();
      expect(terminals.map((t) => t.name)).toEqual(["Second", "Third", "First"]);
    });
  });

  describe("Observer Pattern", () => {
    it("should notify listeners when terminal is added", () => {
      const callback = vi.fn();
      state.onChange(callback);

      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      expect(callback).toHaveBeenCalledTimes(1);
    });

    it("should notify listeners when terminal is removed", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const callback = vi.fn();
      state.onChange(callback);

      state.removeTerminal("terminal-1");

      expect(callback).toHaveBeenCalledTimes(1);
    });

    it("should allow unsubscribing from changes", () => {
      const callback = vi.fn();
      const unsubscribe = state.onChange(callback);

      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      expect(callback).toHaveBeenCalledTimes(1);

      unsubscribe();

      state.addTerminal({
        id: "terminal-2",
        name: "Terminal 2",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      // Should still be 1, not called again after unsubscribe
      expect(callback).toHaveBeenCalledTimes(1);
    });
  });

  describe("Workspace Management", () => {
    it("should set workspace path", () => {
      state.setWorkspace("/path/to/workspace");

      expect(state.getWorkspace()).toBe("/path/to/workspace");
      expect(state.getDisplayedWorkspace()).toBe("/path/to/workspace");
    });

    it("should set displayed workspace independently", () => {
      state.setDisplayedWorkspace("/invalid/path");

      expect(state.getDisplayedWorkspace()).toBe("/invalid/path");
      expect(state.getWorkspace()).toBeNull();
    });
  });

  describe("Terminal Numbering", () => {
    it("should increment terminal number", () => {
      expect(state.getNextTerminalNumber()).toBe(1);
      expect(state.getNextTerminalNumber()).toBe(2);
      expect(state.getNextTerminalNumber()).toBe(3);
    });

    it("should set terminal number", () => {
      state.setNextTerminalNumber(10);
      expect(state.getNextTerminalNumber()).toBe(10);
      expect(state.getNextTerminalNumber()).toBe(11);
    });

    it("should get current terminal number without incrementing", () => {
      state.setNextTerminalNumber(5);
      expect(state.getCurrentTerminalNumber()).toBe(5);
      expect(state.getCurrentTerminalNumber()).toBe(5);
    });
  });

  describe("Role Management", () => {
    it("should set terminal role", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.setTerminalRole("terminal-1", "worker", {
        roleFile: "worker.md",
        targetInterval: 0,
      });

      const terminal = state.getTerminals()[0];
      expect(terminal.role).toBe("worker");
      expect(terminal.roleConfig).toEqual({
        roleFile: "worker.md",
        targetInterval: 0,
      });
    });

    it("should set terminal theme", () => {
      state.addTerminal({
        id: "terminal-1",
        name: "Terminal 1",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      state.setTerminalTheme("terminal-1", "ocean");

      const terminal = state.getTerminals()[0];
      expect(terminal.theme).toBe("ocean");
    });
  });
});
