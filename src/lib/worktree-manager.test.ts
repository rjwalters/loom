import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { type GitIdentity, setupWorktreeForAgent } from "./worktree-manager";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
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

    it("should handle invoke errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("Terminal not found"));

      await expect(setupWorktreeForAgent("test-terminal-12", "/path/to/workspace")).rejects.toThrow(
        "Terminal not found"
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
});
