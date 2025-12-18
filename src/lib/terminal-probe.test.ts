import { describe, expect, test } from "vitest";
import { generateCommandProbe, generateProbeCommand, parseProbeResponse } from "./terminal-probe";

describe("terminal-probe", () => {
  describe("generateProbeCommand", () => {
    test("should generate bash-compatible comment probe", () => {
      const cmd = generateProbeCommand();
      expect(cmd).toContain("# Terminal Probe");
      expect(cmd).toContain("Are you an AI agent?");
      expect(cmd).toContain("AGENT:<role>:<primary-task>");
      expect(cmd).toContain("true");
    });

    test("should end with true command", () => {
      const cmd = generateProbeCommand();
      expect(cmd.trim().endsWith("true")).toBe(true);
    });

    test("should be multi-line with comment and command", () => {
      const cmd = generateProbeCommand();
      const lines = cmd.split("\n");
      expect(lines.length).toBeGreaterThanOrEqual(2);
      expect(lines[0]).toMatch(/^#/); // First line is comment
      expect(lines[lines.length - 1].trim()).toBe("true"); // Last line is true
    });
  });

  describe("generateCommandProbe", () => {
    test("should generate command-based probe", () => {
      const cmd = generateCommandProbe();
      expect(cmd).toContain("command -v");
      expect(cmd).toContain("PROBE:");
    });

    test("should check for claude command", () => {
      const cmd = generateCommandProbe();
      expect(cmd).toContain("claude");
    });
  });

  describe("parseProbeResponse", () => {
    describe("structured AGENT responses", () => {
      test("should detect agent with structured response", () => {
        const output = "AGENT:Worker:implements-loom-ready-issues";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("implements-loom-ready-issues");
        expect(result.raw).toBe(output);
      });

      test("should parse reviewer agent response", () => {
        const output = "AGENT:Reviewer:reviews-PRs-with-loom-review-requested";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Reviewer");
        expect(result.task).toBe("reviews-PRs-with-loom-review-requested");
      });

      test("should parse architect agent response", () => {
        const output = "AGENT:Architect:proposes-system-improvements";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Architect");
        expect(result.task).toBe("proposes-system-improvements");
      });

      test("should handle spaces in role and task", () => {
        const output = "AGENT: Worker : implements issue 222 ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("implements issue 222");
      });

      test("should handle multi-word task descriptions", () => {
        const output = "AGENT:Curator:monitors issues and adds loom:ready label";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Curator");
        expect(result.task).toBe("monitors issues and adds loom:ready label");
      });

      test("should handle idle agent response", () => {
        const output = "AGENT:Worker:idle-awaiting-work";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("idle-awaiting-work");
      });
    });

    describe("command-based probe responses", () => {
      test("should detect PROBE:AGENT response", () => {
        const output = "PROBE:AGENT:claude";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.raw).toBe(output);
      });

      test("should detect PROBE:SHELL response", () => {
        const output = "PROBE:SHELL:bash";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
        expect(result.raw).toBe(output);
      });
    });

    describe("shell detection", () => {
      test("should detect shell from empty output", () => {
        const result = parseProbeResponse("");
        expect(result.type).toBe("shell");
      });

      test("should detect shell from whitespace-only output", () => {
        const result = parseProbeResponse("   \n  \n  ");
        expect(result.type).toBe("shell");
      });

      test("should detect shell from true command output", () => {
        const result = parseProbeResponse("true");
        expect(result.type).toBe("shell");
      });

      test("should detect shell from dollar prompt", () => {
        const result = parseProbeResponse("$ ");
        expect(result.type).toBe("shell");
      });

      test("should detect shell from bash prompt pattern", () => {
        const result = parseProbeResponse("bash-5.2$ ");
        expect(result.type).toBe("shell");
      });

      test("should detect shell from multi-line prompt pattern", () => {
        const output = "$ true\n$";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
      });

      test("should detect shell from hash prompt (root)", () => {
        const result = parseProbeResponse("# ");
        expect(result.type).toBe("shell");
      });
    });

    describe("natural language agent responses", () => {
      test("should detect agent from 'I am' response", () => {
        const output = "I am an AI agent working as a reviewer in this terminal.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });

      test("should detect agent from 'I'm' response", () => {
        const output = "I'm an assistant helping with code reviews.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });

      test("should detect agent from role description", () => {
        const output = "My role is to review pull requests and provide feedback.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });

      test("should detect agent from Claude Code mention", () => {
        const output = "This is Claude Code running in worker mode.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });

      test("should detect agent with role context", () => {
        const output = "Currently operating as a Worker agent.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });

      test("should detect agent with task description", () => {
        const output = "My task is to implement features from GitHub issues.";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
      });
    });

    describe("ambiguous and unknown responses", () => {
      test("should return unknown for ambiguous output", () => {
        const result = parseProbeResponse("random output 123");
        expect(result.type).toBe("unknown");
      });

      test("should return unknown for error messages", () => {
        const result = parseProbeResponse("command not found: something");
        expect(result.type).toBe("unknown");
      });

      test("should return unknown for generic text", () => {
        const result = parseProbeResponse("hello world");
        expect(result.type).toBe("unknown");
      });

      test("should not detect standalone role name as agent", () => {
        // Without context words like "as", "a", "the", standalone role names
        // should not be detected as agents
        const result = parseProbeResponse("Worker");
        expect(result.type).toBe("unknown");
      });
    });

    describe("edge cases", () => {
      test("should handle probe response with extra whitespace", () => {
        const output = "  AGENT:Worker:implements-features  \n\n";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
      });

      test("should handle probe response with newlines", () => {
        const output = "AGENT:Reviewer:reviews-code\n$ ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Reviewer");
      });

      test("should preserve raw output in all cases", () => {
        const output = "test output";
        const result = parseProbeResponse(output);
        expect(result.raw).toBe(output);
      });

      test("should handle case-sensitive AGENT keyword", () => {
        // Lowercase "agent:" should not match structured format
        const result = parseProbeResponse("agent:Worker:task");
        expect(result.type).toBe("unknown");
      });

      test("should handle malformed AGENT response missing task", () => {
        // Must have both role AND task (two colons)
        const result = parseProbeResponse("AGENT:Worker");
        expect(result.type).toBe("unknown");
      });

      test("should handle malformed AGENT response missing role", () => {
        const result = parseProbeResponse("AGENT::task");
        // Regex requires non-empty role field ([^:]+), so this won't match
        expect(result.type).toBe("unknown");
      });
    });

    describe("real-world scenarios", () => {
      test("should handle Claude Code bypass permissions prompt echo", () => {
        // When we send "2\n" to bypass permissions, terminal might echo it
        const output = "2\n$ ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
      });

      test("should handle tmux session startup output", () => {
        const output = "[detached (from session loom-terminal-1)]\n$ ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
      });

      test("should handle zsh prompt pattern", () => {
        const output = "% ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
      });

      test("should differentiate between shell echo and agent response", () => {
        // Shell echoing the probe command should still be detected as shell
        const output = "# Terminal Probe: Are you an AI agent?\ntrue\n$ ";
        const result = parseProbeResponse(output);
        expect(result.type).toBe("shell");
      });
    });
  });

  describe("Separation of Concerns (terminal-probe vs health-monitor)", () => {
    /**
     * These tests verify that terminal-probe.ts maintains its architectural separation:
     * - terminal-probe: CHECKER - determines terminal TYPE via probe commands (stateless)
     * - health-monitor: SCHEDULER - decides WHEN to check, uses IPC for health (stateful)
     */

    test("functions are stateless (pure functions)", () => {
      // generateProbeCommand should always return the same command
      const cmd1 = generateProbeCommand();
      const cmd2 = generateProbeCommand();
      expect(cmd1).toBe(cmd2);

      // generateCommandProbe should always return the same command
      const probe1 = generateCommandProbe();
      const probe2 = generateCommandProbe();
      expect(probe1).toBe(probe2);
    });

    test("parseProbeResponse is a pure function (same input = same output)", () => {
      const output = "AGENT:Worker:implements-issues";

      // Multiple calls with same input should produce identical results
      const result1 = parseProbeResponse(output);
      const result2 = parseProbeResponse(output);

      expect(result1.type).toBe(result2.type);
      expect(result1.role).toBe(result2.role);
      expect(result1.task).toBe(result2.task);
    });

    test("module has no scheduling or timer logic", () => {
      // terminal-probe exports only pure functions, no classes or singletons
      // This is verified by the fact that we can call functions without initialization

      // No setup required - functions work immediately
      expect(() => generateProbeCommand()).not.toThrow();
      expect(() => generateCommandProbe()).not.toThrow();
      expect(() => parseProbeResponse("test")).not.toThrow();
    });

    test("probe functions have no side effects", () => {
      // Calling generate functions multiple times shouldn't change anything
      const before = generateProbeCommand();

      // Call many times
      for (let i = 0; i < 100; i++) {
        generateProbeCommand();
        generateCommandProbe();
        parseProbeResponse("AGENT:Test:task");
      }

      const after = generateProbeCommand();
      expect(before).toBe(after);
    });
  });
});
