import { describe, expect, test } from "vitest";
import { generateCommandProbe, generateProbeCommand, parseProbeResponse } from "./terminal-probe";

describe("terminal-probe", () => {
  describe("generateProbeCommand", () => {
    test("should generate bash-compatible comment probe", () => {
      const cmd = generateProbeCommand();

      // Should contain comment with probe question
      expect(cmd).toContain("# Terminal Probe");
      expect(cmd).toContain("Are you an AI agent?");
      expect(cmd).toContain("AGENT:<role>:<primary-task>");

      // Should end with true command
      expect(cmd).toContain("true");

      // Should be multiline (comment + command)
      expect(cmd.split("\n")).toHaveLength(2);
    });

    test("should not contain shell metacharacters that could cause injection", () => {
      const cmd = generateProbeCommand();

      // Should not have dangerous characters
      expect(cmd).not.toMatch(/[;&|`$()]/);
    });
  });

  describe("generateCommandProbe", () => {
    test("should generate command-based probe", () => {
      const cmd = generateCommandProbe();

      // Should use command -v to check for claude
      expect(cmd).toContain("command -v claude");

      // Should have conditional output
      expect(cmd).toContain("PROBE:AGENT");
      expect(cmd).toContain("PROBE:SHELL");
    });
  });

  describe("parseProbeResponse", () => {
    describe("structured AGENT responses", () => {
      test("should detect agent with full structured response", () => {
        const output = "AGENT:Worker:implements-loom-ready-issues";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("implements-loom-ready-issues");
        expect(result.raw).toBe(output);
      });

      test("should detect agent with Reviewer role", () => {
        const output = "AGENT:Reviewer:reviews-PRs-with-loom-review-requested-label";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Reviewer");
        expect(result.task).toBe("reviews-PRs-with-loom-review-requested-label");
      });

      test("should detect agent with Architect role", () => {
        const output = "AGENT:Architect:proposes-system-improvements";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Architect");
        expect(result.task).toBe("proposes-system-improvements");
      });

      test("should handle extra whitespace in structured response", () => {
        const output = "  AGENT:Worker:fixes-bugs  \n";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("fixes-bugs");
      });

      test("should handle colons in task description", () => {
        const output = "AGENT:Worker:implements-feature-x:phase-2";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        // Task includes everything after the second colon
        expect(result.task).toBe("implements-feature-x:phase-2");
      });
    });

    describe("command-based PROBE responses", () => {
      test("should detect agent from PROBE:AGENT response", () => {
        const output = "PROBE:AGENT:rwalters";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.raw).toBe(output);
        // Role/task undefined for command-based probes
        expect(result.role).toBeUndefined();
        expect(result.task).toBeUndefined();
      });

      test("should detect shell from PROBE:SHELL response", () => {
        const output = "PROBE:SHELL:bash";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("shell");
        expect(result.raw).toBe(output);
      });
    });

    describe("natural language agent responses", () => {
      test("should detect agent from natural language with 'I am' pattern", () => {
        const output = "I am an AI agent working as a reviewer in this terminal...";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.raw).toBe(output);
      });

      test("should detect agent from 'I'm an assistant' pattern", () => {
        const output = "I'm an assistant helping with code review tasks.";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
      });

      test("should detect agent from 'working as' pattern", () => {
        const output = "I'm currently working as a Worker agent on issue #123.";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
      });

      test("should detect agent from 'my role' pattern", () => {
        const output = "My role is to review pull requests and provide feedback.";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
      });

      test("should detect agent from Claude Code mention", () => {
        const output = "Claude Code is running in this terminal with Worker configuration.";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
      });

      test("should detect agent from role names with context", () => {
        const outputs = [
          "This terminal is running as a Worker agent.",
          "I am a Reviewer agent active and monitoring for PRs.",
          "Working as an Architect to propose improvements.",
          "This is the Curator terminal maintaining issue quality.",
        ];

        for (const output of outputs) {
          const result = parseProbeResponse(output);
          expect(result.type).toBe("agent");
        }
      });
    });

    describe("shell responses", () => {
      test("should detect shell from empty output", () => {
        const result = parseProbeResponse("");

        expect(result.type).toBe("shell");
        expect(result.raw).toBe("");
      });

      test("should detect shell from whitespace-only output", () => {
        const result = parseProbeResponse("   \n  \n ");

        expect(result.type).toBe("shell");
      });

      test("should detect shell from 'true' command output", () => {
        const result = parseProbeResponse("true");

        expect(result.type).toBe("shell");
      });

      test("should detect shell from prompt pattern", () => {
        const outputs = ["$ ", "$", "# ", "#", "bash-5.0$ "];

        for (const output of outputs) {
          const result = parseProbeResponse(output);
          expect(result.type).toBe("shell");
        }
      });

      test("should detect shell from typical bash session", () => {
        const output = "$ true\n$";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("shell");
      });
    });

    describe("unknown/ambiguous responses", () => {
      test("should return unknown for random output", () => {
        const output = "random output 123";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("unknown");
        expect(result.raw).toBe(output);
      });

      test("should return unknown for command errors", () => {
        const output = "zsh: command not found: 2";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("unknown");
      });

      test("should return unknown for partial structured responses", () => {
        const output = "AGENT:Worker";
        const result = parseProbeResponse(output);

        // Missing task field, doesn't match structured format
        expect(result.type).toBe("unknown");
      });

      test("should return unknown for ambiguous text", () => {
        const output = "Loading configuration...";
        const result = parseProbeResponse(output);

        expect(result.type).toBe("unknown");
      });
    });

    describe("edge cases", () => {
      test("should handle multiline agent responses", () => {
        const output =
          "AGENT:Worker:implements-features\n\nI'm currently working on implementing the probe system.";
        const result = parseProbeResponse(output);

        // Should match the structured format in first line
        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toBe("implements-features");
      });

      test("should handle responses with ANSI escape codes", () => {
        const output = "\x1b[32mAGENT:Worker:tests\x1b[0m";
        const result = parseProbeResponse(output);

        // ANSI codes break the structured format regex (doesn't start with "AGENT:")
        // This is expected behavior - ANSI codes would need to be stripped first
        expect(result.type).toBe("unknown");
      });

      test("should preserve raw output regardless of parsing", () => {
        const testCases = [
          "AGENT:Worker:task",
          "PROBE:SHELL:bash",
          "",
          "unknown output",
          "I am an agent",
        ];

        for (const output of testCases) {
          const result = parseProbeResponse(output);
          expect(result.raw).toBe(output);
        }
      });

      test("should handle case sensitivity in structured responses", () => {
        // Lowercase 'agent' should not match
        const output1 = "agent:Worker:task";
        const result1 = parseProbeResponse(output1);
        expect(result1.type).toBe("unknown");

        // Uppercase 'AGENT' should match
        const output2 = "AGENT:Worker:task";
        const result2 = parseProbeResponse(output2);
        expect(result2.type).toBe("agent");
      });

      test("should handle very long output", () => {
        const longTask = "a".repeat(1000);
        const output = `AGENT:Worker:${longTask}`;
        const result = parseProbeResponse(output);

        expect(result.type).toBe("agent");
        expect(result.role).toBe("Worker");
        expect(result.task).toHaveLength(1000);
      });
    });
  });
});
