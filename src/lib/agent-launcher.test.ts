import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  detectTerminalType,
  launchAgentInTerminal,
  launchCodexAgent,
  launchDeepSeekAgent,
  launchGeminiCLIAgent,
  launchGitHubCopilotAgent,
  launchGrokAgent,
  sendPromptToAgent,
  stopAgentInTerminal,
} from "./agent-launcher";

// Mock Tauri API
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock terminal-probe module
vi.mock("./terminal-probe", () => ({
  generateProbeCommand: vi.fn(() => "# probe command"),
  parseProbeResponse: vi.fn((output: string) => {
    if (output.includes("AGENT_TYPE")) {
      return {
        type: "agent" as const,
        role: "worker",
        task: "implementing feature",
        raw: output,
      };
    }
    if (output.includes("$ ")) {
      return { type: "shell" as const, role: null, task: null, raw: output };
    }
    return { type: "unknown" as const, role: null, task: null, raw: output };
  }),
}));

import { invoke } from "@tauri-apps/api/tauri";
import { generateProbeCommand, parseProbeResponse } from "./terminal-probe";

describe("agent-launcher", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();

    // Default mock implementations
    vi.mocked(invoke).mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe("detectTerminalType", () => {
    it("sends probe command and waits for response", async () => {
      vi.mocked(invoke).mockResolvedValue("AGENT_TYPE: worker\nTASK: implementing feature");

      const promise = detectTerminalType("terminal-1");

      // Fast-forward timers
      await vi.runAllTimersAsync();

      const result = await promise;

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "# probe command\n",
      });

      expect(invoke).toHaveBeenCalledWith("read_terminal_output", {
        id: "terminal-1",
        lines: 10,
      });

      expect(result.type).toBe("agent");
      expect(result.role).toBe("worker");
    });

    it("uses custom wait time", async () => {
      vi.mocked(invoke).mockResolvedValue("");

      // Start the promise - this creates the timer
      const promise = detectTerminalType("terminal-1", 2000);

      // Wait for the promise microtask to complete, creating timers
      await Promise.resolve();

      // Now check timer count
      const timerCount = vi.getTimerCount();
      expect(timerCount).toBeGreaterThanOrEqual(1);

      await vi.runAllTimersAsync();
      await promise;
    });

    it("handles read output errors gracefully", async () => {
      vi.mocked(invoke)
        .mockResolvedValueOnce(undefined) // send_terminal_input
        .mockRejectedValueOnce(new Error("Failed to read"));

      const promise = detectTerminalType("terminal-1");
      await vi.runAllTimersAsync();
      const result = await promise;

      // Should return unknown type on error
      expect(result.type).toBe("unknown");
    });

    it("detects shell type", async () => {
      vi.mocked(invoke).mockResolvedValue("$ ls\nfile1.txt file2.txt");

      const promise = detectTerminalType("terminal-1");
      await vi.runAllTimersAsync();
      const result = await promise;

      expect(result.type).toBe("shell");
    });

    it("generates probe command correctly", async () => {
      vi.mocked(invoke).mockResolvedValue("");

      const promise = detectTerminalType("terminal-1");
      await vi.runAllTimersAsync();
      await promise;

      expect(generateProbeCommand).toHaveBeenCalled();
    });

    it("parses probe response correctly", async () => {
      const mockOutput = "AGENT_TYPE: worker";
      vi.mocked(invoke).mockResolvedValue(mockOutput);

      const promise = detectTerminalType("terminal-1");
      await vi.runAllTimersAsync();
      await promise;

      expect(parseProbeResponse).toHaveBeenCalledWith(mockOutput);
    });
  });

  describe("launchAgentInTerminal", () => {
    const mockRoleContent = "You are a worker agent in {{workspace}}";
    const workspacePath = "/path/to/workspace";
    const worktreePath = "/path/to/workspace/.loom/worktrees/terminal-1";

    beforeEach(() => {
      vi.mocked(invoke).mockImplementation((cmd, _args) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker\nTASK: ready");
        }
        return Promise.resolve(undefined);
      });
    });

    it("launches Claude agent with role file", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should read role file
      expect(invoke).toHaveBeenCalledWith("read_role_file", {
        workspacePath,
        filename: "worker.md",
      });

      // Should send Claude command
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "claude --dangerously-skip-permissions",
      });

      // Should send Enter to execute
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });

    it("replaces template variables in role content", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should send processed prompt with workspace replaced
      const processedPrompt = `You are a worker agent in ${worktreePath}`;
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: processedPrompt,
      });
    });

    it("uses main workspace when worktreePath is empty", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, "");

      await vi.runAllTimersAsync();
      await promise;

      // Should use workspacePath in template
      const processedPrompt = `You are a worker agent in ${workspacePath}`;
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: processedPrompt,
      });
    });

    it("sends bypass permissions acceptance with retry", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should send "2" to accept bypass permissions (3 retries)
      const bypassCalls = vi
        .mocked(invoke)
        .mock.calls.filter((call) => call[1] && (call[1] as any).data === "2");
      expect(bypassCalls.length).toBe(3); // 3 retry attempts
    });

    it("verifies agent launch with terminal probe", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should call read_terminal_output for probe
      const probeCalls = vi
        .mocked(invoke)
        .mock.calls.filter((call) => call[0] === "read_terminal_output");
      expect(probeCalls.length).toBeGreaterThan(0);
    });

    it("logs successful agent verification", async () => {
      // Mock returns agent type
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker");
        }
        return Promise.resolve(undefined);
      });

      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should verify successfully (check parseProbeResponse was called)
      expect(parseProbeResponse).toHaveBeenCalled();
    });

    it("logs error when shell detected instead of agent", async () => {
      // Mock returns shell type
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("$ ls");
        }
        return Promise.resolve(undefined);
      });

      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should complete (error is logged, not thrown)
      expect(parseProbeResponse).toHaveBeenCalledWith("$ ls");
    });

    it("waits appropriate delays between commands", async () => {
      const promise = launchAgentInTerminal("terminal-1", "worker.md", workspacePath, worktreePath);

      // Wait for promise to start and create timers
      await Promise.resolve();

      // Should have multiple timers for various delays
      const timerCount = vi.getTimerCount();
      expect(timerCount).toBeGreaterThanOrEqual(1);

      await vi.runAllTimersAsync();
      await promise;
    });
  });

  describe("stopAgentInTerminal", () => {
    it("sends Ctrl+C to terminal", async () => {
      await stopAgentInTerminal("terminal-1");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\u0003", // Ctrl+C
      });
    });
  });

  describe("sendPromptToAgent", () => {
    it("sends prompt text and Enter", async () => {
      const prompt = "Continue working on the feature";

      await sendPromptToAgent("terminal-1", prompt);

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: prompt,
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });

    it("handles empty prompts", async () => {
      await sendPromptToAgent("terminal-1", "");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "",
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });
  });

  describe("launchGitHubCopilotAgent", () => {
    it("launches GitHub Copilot with gh command", async () => {
      await launchGitHubCopilotAgent("terminal-1");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "gh copilot",
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });
  });

  describe("launchGeminiCLIAgent", () => {
    it("launches Gemini with gemini chat command", async () => {
      await launchGeminiCLIAgent("terminal-1");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "gemini chat",
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });
  });

  describe("launchDeepSeekAgent", () => {
    it("launches DeepSeek with deepseek chat command", async () => {
      await launchDeepSeekAgent("terminal-1");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "deepseek chat",
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });
  });

  describe("launchGrokAgent", () => {
    it("launches Grok with grok chat command", async () => {
      await launchGrokAgent("terminal-1");

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "grok chat",
      });

      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\r",
      });
    });
  });

  describe("launchCodexAgent", () => {
    const mockRoleContent = "You are a Codex agent in {{workspace}}";
    const workspacePath = "/path/to/workspace";
    const worktreePath = "/path/to/workspace/.loom/worktrees/terminal-1";

    beforeEach(() => {
      vi.mocked(invoke).mockImplementation((cmd, _args) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker\nTASK: ready");
        }
        return Promise.resolve(undefined);
      });
    });

    it("launches Codex agent with role file", async () => {
      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should read role file
      expect(invoke).toHaveBeenCalledWith("read_role_file", {
        workspacePath,
        filename: "worker.md",
      });

      // Should send Codex command with heredoc
      const codexCalls = vi
        .mocked(invoke)
        .mock.calls.filter(
          (call) =>
            call[0] === "send_terminal_input" &&
            typeof (call[1] as any)?.data === "string" &&
            (call[1] as any).data.includes("codex --full-auto")
        );
      expect(codexCalls.length).toBeGreaterThan(0);
    });

    it("replaces template variables in Codex role content", async () => {
      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should send command with processed prompt containing worktree path
      const codexCalls = vi
        .mocked(invoke)
        .mock.calls.filter(
          (call) =>
            call[0] === "send_terminal_input" && (call[1] as any)?.data?.includes(worktreePath)
        );
      expect(codexCalls.length).toBeGreaterThan(0);
    });

    it("uses main workspace when worktreePath is empty", async () => {
      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, "");

      await vi.runAllTimersAsync();
      await promise;

      // Should use workspacePath in template
      const codexCalls = vi
        .mocked(invoke)
        .mock.calls.filter(
          (call) =>
            call[0] === "send_terminal_input" &&
            (call[1] as any)?.data?.includes(workspacePath) &&
            (call[1] as any)?.data?.includes("codex")
        );
      expect(codexCalls.length).toBeGreaterThan(0);
    });

    it("verifies Codex agent launch with terminal probe", async () => {
      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should call read_terminal_output for probe
      const probeCalls = vi
        .mocked(invoke)
        .mock.calls.filter((call) => call[0] === "read_terminal_output");
      expect(probeCalls.length).toBeGreaterThan(0);
    });

    it("logs successful Codex agent verification", async () => {
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker");
        }
        return Promise.resolve(undefined);
      });

      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      expect(parseProbeResponse).toHaveBeenCalled();
    });

    it("logs error when shell detected instead of Codex agent", async () => {
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("$ ls");
        }
        return Promise.resolve(undefined);
      });

      const promise = launchCodexAgent("terminal-1", "worker.md", workspacePath, worktreePath);

      await vi.runAllTimersAsync();
      await promise;

      // Should complete (error is logged, not thrown)
      expect(parseProbeResponse).toHaveBeenCalledWith("$ ls");
    });
  });

  describe("Real-world Scenarios", () => {
    it("launches agent, sends prompts, and stops", async () => {
      const mockRoleContent = "You are a worker agent";

      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker");
        }
        return Promise.resolve(undefined);
      });

      // Launch agent
      const launchPromise = launchAgentInTerminal("terminal-1", "worker.md", "/workspace", "");
      await vi.runAllTimersAsync();
      await launchPromise;

      vi.clearAllMocks();

      // Send prompt
      await sendPromptToAgent("terminal-1", "Work on feature");
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "Work on feature",
      });

      // Stop agent
      await stopAgentInTerminal("terminal-1");
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-1",
        data: "\u0003",
      });
    });

    it("launches different agent types sequentially", async () => {
      const mockRoleContent = "You are a worker agent";

      // Setup mock for Claude launch
      vi.mocked(invoke).mockImplementation((cmd) => {
        if (cmd === "read_role_file") {
          return Promise.resolve(mockRoleContent);
        }
        if (cmd === "read_terminal_output") {
          return Promise.resolve("AGENT_TYPE: worker");
        }
        return Promise.resolve(undefined);
      });

      // Launch Claude
      const claudePromise = launchAgentInTerminal("terminal-1", "worker.md", "/workspace", "");
      await vi.runAllTimersAsync();
      await claudePromise;

      vi.clearAllMocks();

      // Launch Copilot in different terminal
      await launchGitHubCopilotAgent("terminal-2");
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-2",
        data: "gh copilot",
      });

      vi.clearAllMocks();

      // Launch Gemini in third terminal
      await launchGeminiCLIAgent("terminal-3");
      expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
        id: "terminal-3",
        data: "gemini chat",
      });
    });

    it("handles rapid probe checks", async () => {
      vi.mocked(invoke).mockResolvedValue("AGENT_TYPE: worker");

      const probes = [
        detectTerminalType("terminal-1"),
        detectTerminalType("terminal-2"),
        detectTerminalType("terminal-3"),
      ];

      await vi.runAllTimersAsync();
      const results = await Promise.all(probes);

      expect(results).toHaveLength(3);
      expect(results.every((r) => r.type === "agent")).toBe(true);
    });
  });
});
