import { invoke } from "@tauri-apps/api/core";
import { Command } from "@tauri-apps/plugin-shell";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  createWorktreeDirect,
  enrichWorktreeError,
  type GitIdentity,
  setupWorktreeForAgent,
} from "./worktree-manager";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// Mock Tauri shell plugin Command
vi.mock("@tauri-apps/plugin-shell", () => ({
  Command: {
    create: vi.fn(),
  },
}));

describe("worktree-manager", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe("setupWorktreeForAgent", () => {
    it("should create worktree directory structure", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      const worktreePath = await setupWorktreeForAgent("test-terminal-1", "/path/to/workspace");

      // Check mkdir was called
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-1",
        data: 'mkdir -p "/path/to/workspace/.loom/worktrees/test-terminal-1"',
      });

      // Check Enter was sent after mkdir
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-1",
        data: "\r",
      });

      expect(worktreePath).toBe("/path/to/workspace/.loom/worktrees/test-terminal-1");
    });

    it("should create git worktree from HEAD", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-2", "/path/to/workspace");

      // Check git worktree add was called with -b flag for branch isolation
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-2",
        data: 'git worktree add -b "worktree/test-terminal-2" "/path/to/workspace/.loom/worktrees/test-terminal-2" HEAD',
      });

      // Check Enter was sent after git command
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-2",
        data: "\r",
      });
    });

    it("should change to worktree directory", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-3", "/path/to/workspace");

      // Check cd command was called
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-3",
        data: 'cd "/path/to/workspace/.loom/worktrees/test-terminal-3"',
      });

      // Check Enter was sent after cd
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-3",
        data: "\r",
      });
    });

    it("should configure git identity when provided", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      const gitIdentity: GitIdentity = {
        name: "Test User",
        email: "test@example.com",
      };

      await setupWorktreeForAgent("test-terminal-4", "/path/to/workspace", gitIdentity);

      // Check git config user.name
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-4",
        data: 'git config user.name "Test User"',
      });

      // Check git config user.email
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-4",
        data: 'git config user.email "test@example.com"',
      });

      // Check identity confirmation message
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-4",
        data: 'echo "✓ Git identity configured: Test User <test@example.com>"',
      });
    });

    it("should not configure git identity when not provided", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-5", "/path/to/workspace");

      // Should NOT have called git config commands
      const calls = vi.mocked(invoke).mock.calls;
      const gitConfigCalls = calls.filter(
        (call) =>
          call[1] &&
          typeof call[1] === "object" &&
          "data" in call[1] &&
          typeof call[1].data === "string" &&
          call[1].data.includes("git config")
      );

      expect(gitConfigCalls).toHaveLength(0);
    });

    it("should show success message", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-6", "/path/to/workspace");

      // Check success message
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-6",
        data: 'echo "✓ Worktree ready at /path/to/workspace/.loom/worktrees/test-terminal-6"',
      });
    });

    it("should execute commands in correct order", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      const gitIdentity: GitIdentity = {
        name: "Test User",
        email: "test@example.com",
      };

      await setupWorktreeForAgent("test-terminal-7", "/path/to/workspace", gitIdentity);

      // Get all command calls (filter out \r Enter commands)
      const calls = vi.mocked(invoke).mock.calls;
      const commandCalls = calls
        .filter(
          (call): call is [string, { data: string }] =>
            call[1] !== undefined &&
            typeof call[1] === "object" &&
            "data" in call[1] &&
            typeof call[1].data === "string" &&
            call[1].data !== "\r"
        )
        .map((call) => call[1].data);

      // Expected order of commands
      const expectedCommands = [
        'mkdir -p "/path/to/workspace/.loom/worktrees/test-terminal-7"',
        'git worktree add -b "worktree/test-terminal-7" "/path/to/workspace/.loom/worktrees/test-terminal-7" HEAD',
        'cd "/path/to/workspace/.loom/worktrees/test-terminal-7"',
        'git config user.name "Test User"',
        'git config user.email "test@example.com"',
        'echo "✓ Git identity configured: Test User <test@example.com>"',
        'echo "✓ Worktree ready at /path/to/workspace/.loom/worktrees/test-terminal-7"',
      ];

      expect(commandCalls).toEqual(expectedCommands);
    });

    it("should handle workspace paths with spaces", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-8", "/path/with spaces/workspace");

      // Check that paths are properly quoted
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-8",
        data: 'mkdir -p "/path/with spaces/workspace/.loom/worktrees/test-terminal-8"',
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-8",
        data: 'git worktree add -b "worktree/test-terminal-8" "/path/with spaces/workspace/.loom/worktrees/test-terminal-8" HEAD',
      });
    });

    it("should handle terminal IDs with special characters", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-9-special_chars", "/path/to/workspace");

      const worktreePath = await setupWorktreeForAgent(
        "test-terminal-9-special_chars",
        "/path/to/workspace"
      );

      expect(worktreePath).toBe("/path/to/workspace/.loom/worktrees/test-terminal-9-special_chars");
    });

    it("should handle git identity with special characters", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      const gitIdentity: GitIdentity = {
        name: 'Test "User" O\'Reilly',
        email: "test+tag@example.com",
      };

      await setupWorktreeForAgent("test-terminal-10", "/path/to/workspace", gitIdentity);

      // Check that special characters are passed through
      // (shell quoting handles them)
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-10",
        data: 'git config user.name "Test "User" O\'Reilly"',
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "test-terminal-10",
        data: 'git config user.email "test+tag@example.com"',
      });
    });

    it("should send Enter after each command", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      await setupWorktreeForAgent("test-terminal-11", "/path/to/workspace");

      // Count how many Enter commands were sent
      const calls = vi.mocked(invoke).mock.calls;
      const enterCalls = calls.filter(
        (call) =>
          call[1] && typeof call[1] === "object" && "data" in call[1] && call[1].data === "\r"
      );

      // Should have Enter for: mkdir, git worktree, cd, echo success
      expect(enterCalls.length).toBeGreaterThanOrEqual(4);
    });

    it("should return the correct worktree path", async () => {
      vi.mocked(invoke).mockResolvedValue(undefined);

      const result1 = await setupWorktreeForAgent("terminal-a", "/workspace/path");
      expect(result1).toBe("/workspace/path/.loom/worktrees/terminal-a");

      const result2 = await setupWorktreeForAgent("terminal-b", "/different/workspace");
      expect(result2).toBe("/different/workspace/.loom/worktrees/terminal-b");
    });

    it("should handle invoke errors gracefully and enrich them", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("Terminal not found"));

      await expect(setupWorktreeForAgent("test-terminal-12", "/path/to/workspace")).rejects.toThrow(
        "Cannot create worktree: terminal 'test-terminal-12' not ready"
      );
    });

    it("should delay between commands", async () => {
      vi.useFakeTimers();
      vi.mocked(invoke).mockResolvedValue(undefined);

      const setupPromise = setupWorktreeForAgent("test-terminal-13", "/path/to/workspace");

      // Fast-forward time to resolve delays
      await vi.runAllTimersAsync();

      await setupPromise;

      expect(invoke).toHaveBeenCalled();

      vi.useRealTimers();
    });
  });

  describe("createWorktreeDirect", () => {
    // Helper to create a mock Command.execute result
    const mockExecuteResult = (code: number, stdout = "", stderr = "") => ({
      execute: vi.fn().mockResolvedValue({ code, stdout, stderr }),
    });

    beforeEach(() => {
      vi.mocked(Command.create).mockReturnValue(mockExecuteResult(0));
    });

    it("should create worktree directory using Command.create", async () => {
      const worktreePath = await createWorktreeDirect("terminal-1", "/path/to/workspace");

      // Check mkdir was called
      expect(Command.create).toHaveBeenCalledWith(
        "mkdir",
        ["-p", "/path/to/workspace/.loom/worktrees"],
        { cwd: "/path/to/workspace" }
      );

      expect(worktreePath).toBe("/path/to/workspace/.loom/worktrees/terminal-1");
    });

    it("should create git worktree with correct branch name", async () => {
      await createWorktreeDirect("terminal-2", "/path/to/workspace");

      // Check git worktree add was called
      expect(Command.create).toHaveBeenCalledWith(
        "git",
        [
          "worktree",
          "add",
          "-b",
          "worktree/terminal-2",
          "/path/to/workspace/.loom/worktrees/terminal-2",
          "HEAD",
        ],
        { cwd: "/path/to/workspace" }
      );
    });

    it("should check if worktree exists before creating", async () => {
      await createWorktreeDirect("terminal-3", "/path/to/workspace");

      // Check test -d was called to check if directory exists
      expect(Command.create).toHaveBeenCalledWith(
        "test",
        ["-d", "/path/to/workspace/.loom/worktrees/terminal-3"],
        { cwd: "/path/to/workspace" }
      );
    });

    it("should remove existing worktree if it exists", async () => {
      // Mock test -d to return success (directory exists)
      vi.mocked(Command.create)
        .mockReturnValueOnce(mockExecuteResult(0)) // mkdir
        .mockReturnValueOnce(mockExecuteResult(0)) // test -d returns 0 (exists)
        .mockReturnValueOnce(mockExecuteResult(0)) // git worktree remove
        .mockReturnValueOnce(mockExecuteResult(0)) // git branch -D
        .mockReturnValueOnce(mockExecuteResult(0)); // git worktree add

      await createWorktreeDirect("terminal-4", "/path/to/workspace");

      // Check git worktree remove was called
      expect(Command.create).toHaveBeenCalledWith(
        "git",
        ["worktree", "remove", "/path/to/workspace/.loom/worktrees/terminal-4", "--force"],
        { cwd: "/path/to/workspace" }
      );

      // Check git branch -D was called
      expect(Command.create).toHaveBeenCalledWith("git", ["branch", "-D", "worktree/terminal-4"], {
        cwd: "/path/to/workspace",
      });
    });

    it("should not remove worktree if it does not exist", async () => {
      // Mock test -d to return failure (directory does not exist)
      vi.mocked(Command.create)
        .mockReturnValueOnce(mockExecuteResult(0)) // mkdir
        .mockReturnValueOnce(mockExecuteResult(1)) // test -d returns 1 (doesn't exist)
        .mockReturnValueOnce(mockExecuteResult(0)); // git worktree add

      await createWorktreeDirect("terminal-5", "/path/to/workspace");

      // Should not have called git worktree remove
      const removeCall = vi
        .mocked(Command.create)
        .mock.calls.find(
          (call) => call[0] === "git" && call[1]?.[0] === "worktree" && call[1]?.[1] === "remove"
        );
      expect(removeCall).toBeUndefined();
    });

    it("should return the correct worktree path", async () => {
      const result1 = await createWorktreeDirect("terminal-a", "/workspace/path");
      expect(result1).toBe("/workspace/path/.loom/worktrees/terminal-a");

      const result2 = await createWorktreeDirect("terminal-b", "/different/workspace");
      expect(result2).toBe("/different/workspace/.loom/worktrees/terminal-b");
    });

    it("should throw enriched error on git worktree add failure", async () => {
      vi.mocked(Command.create)
        .mockReturnValueOnce(mockExecuteResult(0)) // mkdir
        .mockReturnValueOnce(mockExecuteResult(1)) // test -d (doesn't exist)
        .mockReturnValueOnce(mockExecuteResult(1, "", "fatal: not a git repository")); // git worktree add fails

      await expect(createWorktreeDirect("terminal-6", "/path/to/workspace")).rejects.toThrow(
        "is not a git repository"
      );
    });

    it("should handle workspace paths with spaces", async () => {
      await createWorktreeDirect("terminal-7", "/path/with spaces/workspace");

      expect(Command.create).toHaveBeenCalledWith(
        "git",
        [
          "worktree",
          "add",
          "-b",
          "worktree/terminal-7",
          "/path/with spaces/workspace/.loom/worktrees/terminal-7",
          "HEAD",
        ],
        { cwd: "/path/with spaces/workspace" }
      );
    });

    it("should not require terminal to exist (no invoke calls)", async () => {
      await createWorktreeDirect("terminal-8", "/path/to/workspace");

      // Should NOT have called send_terminal_input since terminal doesn't exist
      expect(invoke).not.toHaveBeenCalledWith("send_terminal_input", expect.anything());
    });
  });

  describe("enrichWorktreeError", () => {
    const defaultContext = {
      terminalId: "terminal-1",
      workspacePath: "/path/to/workspace",
      worktreePath: "/path/to/workspace/.loom/worktrees/terminal-1",
      branchName: "worktree/terminal-1",
    };

    it("should enrich 'Terminal not found' error with actionable message", () => {
      const error = new Error("Terminal not found");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("Cannot create worktree: terminal 'terminal-1' not ready");
      expect(enriched.message).toContain("bug in terminal creation order");
      expect(enriched.message).toContain("issue #734");
    });

    it("should enrich 'not a git repository' error with actionable message", () => {
      const error = new Error("fatal: not a git repository");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain(
        "Cannot create worktree: '/path/to/workspace' is not a git repository"
      );
      expect(enriched.message).toContain("Please initialize git");
    });

    it("should enrich worktree directory exists error with actionable message", () => {
      const error = new Error(
        "fatal: '/path/to/workspace/.loom/worktrees/terminal-1' already exists"
      );
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("Cannot create worktree: directory");
      expect(enriched.message).toContain("already exists");
      expect(enriched.message).toContain("previous session");
    });

    it("should enrich branch name collision error with actionable message", () => {
      const error = new Error("fatal: a branch named 'worktree/terminal-1' already exists");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("Cannot create worktree: branch 'worktree/terminal-1'");
      expect(enriched.message).toContain("already exists");
      expect(enriched.message).toContain("git branch -D worktree/terminal-1");
    });

    it("should enrich permission denied error with actionable message", () => {
      const error = new Error("Permission denied: mkdir failed");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("insufficient permissions");
      expect(enriched.message).toContain("Check file system permissions");
    });

    it("should enrich 'cannot create directory' error with actionable message", () => {
      const error = new Error("mkdir: cannot create directory '/path/to/workspace/.loom'");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("insufficient permissions");
      expect(enriched.message).toContain(defaultContext.worktreePath);
    });

    it("should enrich invalid reference error with actionable message", () => {
      const error = new Error("fatal: invalid reference: HEAD");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("HEAD is invalid or detached");
      expect(enriched.message).toContain("at least one commit");
    });

    it("should enrich 'not a valid ref' error with actionable message", () => {
      const error = new Error("fatal: not a valid ref: HEAD");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("HEAD is invalid or detached");
    });

    it("should provide generic fallback with context for unknown errors", () => {
      const error = new Error("Some unknown git error");
      const enriched = enrichWorktreeError(error, defaultContext);

      expect(enriched.message).toContain("Failed to setup worktree for terminal 'terminal-1'");
      expect(enriched.message).toContain("Some unknown git error");
      expect(enriched.message).toContain(defaultContext.worktreePath);
    });

    it("should handle non-Error objects", () => {
      const enriched = enrichWorktreeError("string error message", defaultContext);

      expect(enriched.message).toContain("string error message");
      expect(enriched).toBeInstanceOf(Error);
    });

    it("should handle null/undefined errors", () => {
      const enrichedNull = enrichWorktreeError(null, defaultContext);
      const enrichedUndefined = enrichWorktreeError(undefined, defaultContext);

      expect(enrichedNull.message).toContain("null");
      expect(enrichedUndefined.message).toContain("undefined");
    });

    it("should preserve terminal ID in all error messages", () => {
      const contexts = [
        { ...defaultContext, terminalId: "custom-terminal-42" },
        { ...defaultContext, terminalId: "terminal-with-special_chars" },
      ];

      for (const context of contexts) {
        const error = new Error("Some error");
        const enriched = enrichWorktreeError(error, context);
        expect(enriched.message).toContain(context.terminalId);
      }
    });

    it("should preserve workspace path in relevant error messages", () => {
      const context = {
        ...defaultContext,
        workspacePath: "/custom/workspace/path",
      };

      const error = new Error("fatal: not a git repository");
      const enriched = enrichWorktreeError(error, context);

      expect(enriched.message).toContain("/custom/workspace/path");
    });
  });
});
