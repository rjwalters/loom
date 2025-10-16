/**
 * Tests for terminal-probe module
 */

import { describe, it, expect } from "vitest";
import {
  generateProbeCommand,
  generateCommandBasedProbe,
  parseProbeResponse,
  isAgentResponse,
  isShellResponse,
  getProbeDescription,
  type ProbeResponse,
} from "./terminal-probe";

describe("generateProbeCommand", () => {
  it("should generate a valid bash command", () => {
    const probe = generateProbeCommand();
    expect(probe).toContain("# Terminal Probe:");
    expect(probe).toContain("AGENT:<role>:<primary-task>");
    expect(probe).toContain("true");
  });

  it("should not cause errors when executed in bash", () => {
    const probe = generateProbeCommand();
    // Should start with comment (safe in bash)
    expect(probe).toMatch(/^#/);
    // Should end with true command (always succeeds)
    expect(probe).toMatch(/true$/);
  });
});

describe("generateCommandBasedProbe", () => {
  it("should generate command-based probe", () => {
    const probe = generateCommandBasedProbe();
    expect(probe).toContain("command -v claude");
    expect(probe).toContain("PROBE:AGENT");
    expect(probe).toContain("PROBE:SHELL");
  });

  it("should use conditional logic", () => {
    const probe = generateCommandBasedProbe();
    expect(probe).toContain("&&");
    expect(probe).toContain("||");
  });
});

describe("parseProbeResponse", () => {
  describe("agent responses", () => {
    it("should parse structured agent response with role and task", () => {
      const output = "AGENT:Worker:implements-loom-ready-issues";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("agent");
      expect(result.role).toBe("Worker");
      expect(result.task).toBe("implements-loom-ready-issues");
      expect(result.raw).toBe(output);
    });

    it("should parse agent response with spaces", () => {
      const output = "AGENT:Reviewer:reviews PRs with loom:review-requested";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("agent");
      expect(result.role).toBe("Reviewer");
      expect(result.task).toBe("reviews PRs with loom:review-requested");
    });

    it("should parse PROBE:AGENT format", () => {
      const output = "PROBE:AGENT:claude";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("agent");
      expect(result.raw).toBe(output);
    });

    it("should handle agent response with surrounding text", () => {
      const output = "Some prompt text\nAGENT:Curator:maintains-issues\nMore text";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("agent");
      expect(result.role).toBe("Curator");
      expect(result.task).toBe("maintains-issues");
    });

    it("should handle multi-line agent response", () => {
      const output = "I'm Claude Code.\n\nAGENT:Architect:designs-system-architecture\n\nHow can I help?";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("agent");
      expect(result.role).toBe("Architect");
      expect(result.task).toBe("designs-system-architecture");
    });
  });

  describe("shell responses", () => {
    it("should detect PROBE:SHELL format", () => {
      const output = "PROBE:SHELL:bash";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
      expect(result.raw).toBe(output);
    });

    it("should detect empty output as shell", () => {
      const output = "";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
    });

    it("should detect bash prompt as shell", () => {
      const output = "user@host:~$ ";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
    });

    it("should detect zsh prompt as shell", () => {
      const output = "% ";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
    });

    it("should detect fish prompt as shell", () => {
      const output = "> ";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
    });

    it("should detect whitespace-only output as shell", () => {
      const output = "   \n  \n  ";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("shell");
    });
  });

  describe("unknown responses", () => {
    it("should mark unclear output as unknown", () => {
      const output = "Some random terminal output without clear indicators";
      const result = parseProbeResponse(output);

      expect(result.type).toBe("unknown");
      expect(result.raw).toBe(output);
    });

    it("should handle malformed agent response", () => {
      const output = "AGENT:NoTaskSpecified";
      const result = parseProbeResponse(output);

      // Should not match the AGENT:role:task pattern
      expect(result.type).toBe("unknown");
    });
  });
});

describe("isAgentResponse", () => {
  it("should return true for agent responses", () => {
    const response: ProbeResponse = {
      type: "agent",
      role: "Worker",
      task: "implements-features",
      raw: "AGENT:Worker:implements-features",
    };

    expect(isAgentResponse(response)).toBe(true);
  });

  it("should return false for shell responses", () => {
    const response: ProbeResponse = {
      type: "shell",
      raw: "PROBE:SHELL:bash",
    };

    expect(isAgentResponse(response)).toBe(false);
  });

  it("should return false for unknown responses", () => {
    const response: ProbeResponse = {
      type: "unknown",
      raw: "random output",
    };

    expect(isAgentResponse(response)).toBe(false);
  });
});

describe("isShellResponse", () => {
  it("should return true for shell responses", () => {
    const response: ProbeResponse = {
      type: "shell",
      raw: "$ ",
    };

    expect(isShellResponse(response)).toBe(true);
  });

  it("should return false for agent responses", () => {
    const response: ProbeResponse = {
      type: "agent",
      role: "Reviewer",
      task: "reviews-prs",
      raw: "AGENT:Reviewer:reviews-prs",
    };

    expect(isShellResponse(response)).toBe(false);
  });
});

describe("getProbeDescription", () => {
  it("should describe agent with role and task", () => {
    const response: ProbeResponse = {
      type: "agent",
      role: "Worker",
      task: "implements-loom-ready-issues",
      raw: "",
    };

    const desc = getProbeDescription(response);
    expect(desc).toBe("AI Agent - Worker (implements-loom-ready-issues)");
  });

  it("should describe agent without details", () => {
    const response: ProbeResponse = {
      type: "agent",
      raw: "PROBE:AGENT:user",
    };

    const desc = getProbeDescription(response);
    expect(desc).toBe("AI Agent");
  });

  it("should describe shell", () => {
    const response: ProbeResponse = {
      type: "shell",
      raw: "$ ",
    };

    const desc = getProbeDescription(response);
    expect(desc).toBe("Bash Shell");
  });

  it("should describe unknown type", () => {
    const response: ProbeResponse = {
      type: "unknown",
      raw: "unclear output",
    };

    const desc = getProbeDescription(response);
    expect(desc).toBe("Unknown Terminal Type");
  });
});
