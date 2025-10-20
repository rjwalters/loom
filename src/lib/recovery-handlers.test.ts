import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  handleAttachToSession,
  handleKillSession,
  handleRecoverAttachSession,
  handleRecoverNewSession,
  type RecoveryDependencies,
} from "./recovery-handlers";
import { AppState, TerminalStatus } from "./state";

// Mock Tauri APIs
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/dialog", () => ({
  ask: vi.fn(),
}));

import { ask } from "@tauri-apps/api/dialog";
import { invoke } from "@tauri-apps/api/tauri";

// Helper to assert JSON structured log messages
function assertLogMessage(spy: any, expectedMessage: string) {
  const calls = spy.mock.calls;
  const found = calls.some((call: any[]) => {
    try {
      const log = JSON.parse(call[0]);
      return log.message === expectedMessage;
    } catch {
      return false;
    }
  });
  expect(found, `Expected log with message: ${expectedMessage}`).toBe(true);
}

function assertLogContains(spy: any, expectedSubstring: string) {
  const calls = spy.mock.calls;
  const found = calls.some((call: any[]) => {
    try {
      const log = JSON.parse(call[0]);
      return log.message && log.message.includes(expectedSubstring);
    } catch {
      return false;
    }
  });
  expect(found, `Expected log containing: ${expectedSubstring}`).toBe(true);
}
describe("recovery-handlers", () => {
  let state: AppState;
  let mockGenerateNextConfigId: ReturnType<typeof vi.fn>;
  let mockSaveCurrentConfig: ReturnType<typeof vi.fn>;
  let deps: RecoveryDependencies;
  let alertSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    state = new AppState();
    state.setWorkspace("/test/workspace");

    mockGenerateNextConfigId = vi.fn().mockReturnValue("config-123");
    mockSaveCurrentConfig = vi.fn().mockResolvedValue(undefined);

    deps = {
      state,
      generateNextConfigId: mockGenerateNextConfigId,
      saveCurrentConfig: mockSaveCurrentConfig,
    };

    alertSpy = vi.spyOn(window, "alert").mockImplementation(() => {});

    vi.clearAllMocks();
  });

  describe("handleRecoverNewSession", () => {
    it("creates new session and updates terminal", async () => {
      state.addTerminal({
        id: "term-old",
        name: "Test Terminal",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockResolvedValue("term-new");

      await handleRecoverNewSession("term-old", deps);

      expect(invoke).toHaveBeenCalledWith("create_terminal", {
        configId: "config-123",
        name: "Test Terminal",
        workingDir: "/test/workspace",
        role: "default",
        instanceNumber: 1,
      });

      expect(state.getTerminal("term-new")).toBeDefined();
      expect(state.getTerminal("term-old")).toBeUndefined();
      expect(state.getPrimary()?.id).toBe("term-new");
      expect(mockSaveCurrentConfig).toHaveBeenCalled();
    });

    it("preserves terminal role when creating new session", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Worker",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        role: "claude-code-worker",
        missingSession: true,
      });

      vi.mocked(invoke).mockResolvedValue("term-new");

      await handleRecoverNewSession("term-1", deps);

      expect(invoke).toHaveBeenCalledWith("create_terminal", {
        configId: "config-123",
        name: "Worker",
        workingDir: "/test/workspace",
        role: "claude-code-worker",
        instanceNumber: 1,
      });
    });

    it("shows alert when no workspace selected", async () => {
      state.setWorkspace("");

      await handleRecoverNewSession("term-1", deps);

      expect(alertSpy).toHaveBeenCalledWith("Cannot recover: no workspace selected");
      expect(invoke).not.toHaveBeenCalled();
    });

    it("shows alert when terminal not found", async () => {
      await handleRecoverNewSession("nonexistent", deps);

      expect(alertSpy).toHaveBeenCalledWith("Cannot recover: terminal not found");
      expect(invoke).not.toHaveBeenCalled();
    });

    it("handles invoke error gracefully", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockRejectedValue(new Error("Failed to create terminal"));

      await handleRecoverNewSession("term-1", deps);

      expect(alertSpy).toHaveBeenCalledWith(
        expect.stringContaining("Failed to create new session")
      );
    });

    it("clears missing session flag on recovered terminal", async () => {
      state.addTerminal({
        id: "term-old",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockResolvedValue("term-new");

      await handleRecoverNewSession("term-old", deps);

      const newTerminal = state.getTerminal("term-new");
      expect(newTerminal?.missingSession).toBeUndefined();
    });
  });

  describe("handleRecoverAttachSession", () => {
    it("loads available sessions", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockResolvedValue(["session-1", "session-2"]);

      await handleRecoverAttachSession("term-1", state);

      expect(invoke).toHaveBeenCalledWith("list_available_sessions");
    });

    it("handles terminal not found", async () => {
      await handleRecoverAttachSession("nonexistent", state);

      // Should not throw, just log error
      expect(invoke).not.toHaveBeenCalled();
    });

    it("handles invoke error gracefully", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockRejectedValue(new Error("Failed to list sessions"));

      await handleRecoverAttachSession("term-1", state);

      expect(alertSpy).toHaveBeenCalledWith(
        expect.stringContaining("Failed to list available sessions")
      );
    });
  });

  describe("handleAttachToSession", () => {
    it("attaches terminal to session and updates state", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockResolvedValue(undefined);

      await handleAttachToSession("term-1", "session-1", deps);

      expect(invoke).toHaveBeenCalledWith("attach_to_session", {
        id: "term-1",
        sessionName: "session-1",
      });

      const terminal = state.getTerminal("term-1");
      expect(terminal?.status).toBe(TerminalStatus.Idle);
      expect(terminal?.missingSession).toBeUndefined();
      expect(mockSaveCurrentConfig).toHaveBeenCalled();
    });

    it("handles terminal not found gracefully", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await handleAttachToSession("nonexistent", "session-1", deps);

      expect(invoke).toHaveBeenCalled();
      // Should not throw even if terminal doesn't exist
    });

    it("handles invoke error gracefully", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Stopped,
        isPrimary: false,
        missingSession: true,
      });

      vi.mocked(invoke).mockRejectedValue(new Error("Failed to attach"));

      await handleAttachToSession("term-1", "session-1", deps);

      expect(alertSpy).toHaveBeenCalledWith(expect.stringContaining("Failed to attach to session"));
    });
  });

  describe("handleKillSession", () => {
    it("kills session when user confirms", async () => {
      vi.mocked(ask).mockResolvedValue(true);
      vi.mocked(invoke).mockResolvedValue(undefined);

      await handleKillSession("session-1", state);

      expect(ask).toHaveBeenCalledWith(
        expect.stringContaining('kill session "session-1"'),
        expect.objectContaining({
          title: "Kill Session",
          type: "warning",
        })
      );

      expect(invoke).toHaveBeenCalledWith("kill_session", { sessionName: "session-1" });
    });

    it("does not kill session when user cancels", async () => {
      vi.mocked(ask).mockResolvedValue(false);

      await handleKillSession("session-1", state);

      expect(ask).toHaveBeenCalled();
      expect(invoke).not.toHaveBeenCalled();
    });

    it("handles invoke error gracefully", async () => {
      vi.mocked(ask).mockResolvedValue(true);
      vi.mocked(invoke).mockRejectedValue(new Error("Failed to kill"));

      await handleKillSession("session-1", state);

      expect(alertSpy).toHaveBeenCalledWith(expect.stringContaining("Failed to kill session"));
    });
  });
});
