import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CircuitOpenError } from "./circuit-breaker";
import { getSessionManager, resetSessionManager, SessionManager } from "./session-manager";

// Mock Tauri invoke
const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

// Mock circuit breaker - note: CircuitOpenError is NOT mocked so instanceof checks work
const mockCircuitBreaker = {
  canAttempt: vi.fn(() => true),
  execute: vi.fn((fn: () => Promise<unknown>) => fn()),
  getState: vi.fn(() => "closed"),
};

vi.mock("./circuit-breaker", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./circuit-breaker")>();
  return {
    ...actual,
    getDaemonCircuitBreaker: () => mockCircuitBreaker,
  };
});

// Mock logger
vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      warn: vi.fn(),
      error: vi.fn(),
    }),
  },
}));

describe("SessionManager", () => {
  let sessionManager: SessionManager;

  beforeEach(() => {
    vi.clearAllMocks();
    resetSessionManager();
    sessionManager = new SessionManager();
    mockCircuitBreaker.canAttempt.mockReturnValue(true);
    mockCircuitBreaker.execute.mockImplementation((fn: () => Promise<unknown>) => fn());
    mockInvoke.mockResolvedValue(undefined);
  });

  afterEach(() => {
    resetSessionManager();
  });

  describe("getSessionId", () => {
    it("returns the fixed session ID", () => {
      expect(sessionManager.getSessionId()).toBe("claude-session");
    });
  });

  describe("launchSession", () => {
    it("creates a terminal with the correct parameters", async () => {
      await sessionManager.launchSession("/test/workspace");

      expect(mockInvoke).toHaveBeenCalledWith("create_terminal", {
        configId: "claude-session",
        name: "Claude Code",
        workingDir: "/test/workspace",
        role: "claude-code-worker",
        instanceNumber: 0,
      });
    });

    it("launches Claude Code CLI after creating terminal", async () => {
      await sessionManager.launchSession("/test/workspace");

      expect(mockInvoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "claude-session",
        data: "claude\n",
      });
    });

    it("sets session state to working after launch", async () => {
      await sessionManager.launchSession("/test/workspace");

      const status = sessionManager.getSessionStatus();
      expect(status.exists).toBe(true);
      expect(status.state).toBe("working");
    });

    it("stores the workspace path", async () => {
      await sessionManager.launchSession("/test/workspace");

      expect(sessionManager.getWorkspacePath()).toBe("/test/workspace");
    });

    it("destroys existing session before relaunch", async () => {
      await sessionManager.launchSession("/first/workspace");
      await sessionManager.launchSession("/second/workspace");

      expect(mockInvoke).toHaveBeenCalledWith("destroy_terminal", {
        id: "claude-session",
      });
    });

    it("throws error when circuit breaker is open", async () => {
      mockCircuitBreaker.canAttempt.mockReturnValue(false);
      mockCircuitBreaker.execute.mockRejectedValue(new CircuitOpenError("daemon-ipc", "open"));

      await expect(sessionManager.launchSession("/test/workspace")).rejects.toThrow(
        "Daemon is unresponsive"
      );
    });
  });

  describe("destroySession", () => {
    it("destroys the terminal", async () => {
      await sessionManager.launchSession("/test/workspace");
      await sessionManager.destroySession();

      expect(mockInvoke).toHaveBeenCalledWith("destroy_terminal", {
        id: "claude-session",
      });
    });

    it("updates session state to stopped", async () => {
      await sessionManager.launchSession("/test/workspace");
      await sessionManager.destroySession();

      const status = sessionManager.getSessionStatus();
      expect(status.exists).toBe(false);
      expect(status.state).toBe("stopped");
    });

    it("does nothing if no session exists", async () => {
      await sessionManager.destroySession();

      expect(mockInvoke).not.toHaveBeenCalledWith("destroy_terminal", expect.anything());
    });
  });

  describe("sendInput", () => {
    it("sends input to the terminal", async () => {
      await sessionManager.launchSession("/test/workspace");
      await sessionManager.sendInput("test input");

      expect(mockInvoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "claude-session",
        data: "test input",
      });
    });

    it("throws error if no session exists", async () => {
      await expect(sessionManager.sendInput("test")).rejects.toThrow(
        "No active session. Call launchSession() first."
      );
    });
  });

  describe("getOutput", () => {
    it("returns empty string if no session exists", async () => {
      const output = await sessionManager.getOutput();
      expect(output).toBe("");
    });

    it("decodes base64 output from terminal", async () => {
      await sessionManager.launchSession("/test/workspace");

      // "Hello World" in base64
      mockInvoke.mockResolvedValueOnce({
        output: "SGVsbG8gV29ybGQ=",
        byte_count: 11,
      });

      const output = await sessionManager.getOutput();
      expect(output).toBe("Hello World");
    });

    it("returns last N lines when lines parameter is provided", async () => {
      await sessionManager.launchSession("/test/workspace");

      // "Line1\nLine2\nLine3" in base64
      mockInvoke.mockResolvedValueOnce({
        output: "TGluZTEKTGluZTIKTGluZTM=",
        byte_count: 17,
      });

      const output = await sessionManager.getOutput(2);
      expect(output).toBe("Line2\nLine3");
    });

    it("updates byte count for incremental polling", async () => {
      await sessionManager.launchSession("/test/workspace");

      mockInvoke.mockResolvedValueOnce({
        output: "SGVsbG8=", // "Hello"
        byte_count: 5,
      });

      await sessionManager.getOutput();

      // Next call should pass the byte count
      mockInvoke.mockResolvedValueOnce({
        output: "V29ybGQ=", // "World"
        byte_count: 10,
      });

      await sessionManager.getOutput();

      expect(mockInvoke).toHaveBeenLastCalledWith("get_terminal_output", {
        id: "claude-session",
        startByte: 5,
      });
    });
  });

  describe("getConversationDir", () => {
    it("returns null if no workspace is set", () => {
      expect(sessionManager.getConversationDir()).toBeNull();
    });

    it("returns .claude directory path in workspace", async () => {
      await sessionManager.launchSession("/test/workspace");
      expect(sessionManager.getConversationDir()).toBe("/test/workspace/.claude");
    });
  });

  describe("isActive", () => {
    it("returns false when session does not exist", () => {
      expect(sessionManager.isActive()).toBe(false);
    });

    it("returns true when session is running", async () => {
      await sessionManager.launchSession("/test/workspace");
      expect(sessionManager.isActive()).toBe(true);
    });

    it("returns false after session is destroyed", async () => {
      await sessionManager.launchSession("/test/workspace");
      await sessionManager.destroySession();
      expect(sessionManager.isActive()).toBe(false);
    });
  });

  describe("setState", () => {
    it("updates the session state", async () => {
      await sessionManager.launchSession("/test/workspace");

      sessionManager.setState("idle");
      expect(sessionManager.getSessionStatus().state).toBe("idle");

      sessionManager.setState("error");
      expect(sessionManager.getSessionStatus().state).toBe("error");
    });

    it("marks session as non-existent when stopped", async () => {
      await sessionManager.launchSession("/test/workspace");

      sessionManager.setState("stopped");
      expect(sessionManager.getSessionStatus().exists).toBe(false);
    });
  });

  describe("resetOutputCounter", () => {
    it("resets the byte counter for full output fetch", async () => {
      await sessionManager.launchSession("/test/workspace");

      // First fetch sets byte count
      mockInvoke.mockResolvedValueOnce({
        output: "SGVsbG8=",
        byte_count: 100,
      });
      await sessionManager.getOutput();

      // Reset the counter
      sessionManager.resetOutputCounter();

      // Next fetch should start from null (beginning)
      mockInvoke.mockResolvedValueOnce({
        output: "V29ybGQ=",
        byte_count: 200,
      });
      await sessionManager.getOutput();

      expect(mockInvoke).toHaveBeenLastCalledWith("get_terminal_output", {
        id: "claude-session",
        startByte: null,
      });
    });
  });
});

describe("getSessionManager", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    resetSessionManager();
  });

  afterEach(() => {
    resetSessionManager();
  });

  it("returns a singleton instance", () => {
    const instance1 = getSessionManager();
    const instance2 = getSessionManager();

    expect(instance1).toBe(instance2);
  });

  it("creates new instance after reset", () => {
    const instance1 = getSessionManager();
    resetSessionManager();
    const instance2 = getSessionManager();

    expect(instance1).not.toBe(instance2);
  });
});
