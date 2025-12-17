/**
 * Unit tests for Zod schema definitions
 */

import { describe, expect, it } from "vitest";
import {
  ActivityEntrySchema,
  ColorThemeSchema,
  GitIdentitySchema,
  InputRequestSchema,
  LoomConfigSchema,
  LoomStateSchema,
  RawLoomConfigSchema,
  RoleMetadataSchema,
  TerminalConfigSchema,
  TerminalStateSchema,
} from "./schemas";

describe("ColorThemeSchema", () => {
  it("validates a valid color theme", () => {
    const theme = {
      name: "Ocean",
      primary: "#3B82F6",
      border: "#1E40AF",
    };
    const result = ColorThemeSchema.safeParse(theme);
    expect(result.success).toBe(true);
  });

  it("allows optional background", () => {
    const theme = {
      name: "Ocean",
      primary: "#3B82F6",
      border: "#1E40AF",
      background: "#0F172A",
    };
    const result = ColorThemeSchema.safeParse(theme);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.background).toBe("#0F172A");
    }
  });

  it("rejects missing required fields", () => {
    const theme = { name: "Ocean" };
    const result = ColorThemeSchema.safeParse(theme);
    expect(result.success).toBe(false);
  });
});

describe("InputRequestSchema", () => {
  it("validates a valid input request", () => {
    const request = {
      id: "req-123",
      prompt: "What should I do next?",
      timestamp: Date.now(),
    };
    const result = InputRequestSchema.safeParse(request);
    expect(result.success).toBe(true);
  });

  it("rejects missing fields", () => {
    const request = { id: "req-123" };
    const result = InputRequestSchema.safeParse(request);
    expect(result.success).toBe(false);
  });
});

describe("TerminalConfigSchema", () => {
  it("validates a minimal terminal config", () => {
    const config = {
      id: "terminal-1",
      name: "Builder",
    };
    const result = TerminalConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
  });

  it("validates a full terminal config", () => {
    const config = {
      id: "terminal-1",
      name: "Builder",
      role: "claude-code-worker",
      roleConfig: {
        workerType: "claude",
        roleFile: "builder.md",
        targetInterval: 300000,
        intervalPrompt: "Continue working",
      },
      theme: "ocean",
      customTheme: {
        name: "Custom Ocean",
        primary: "#3B82F6",
        border: "#1E40AF",
      },
    };
    const result = TerminalConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
  });

  it("rejects empty id", () => {
    const config = {
      id: "",
      name: "Builder",
    };
    const result = TerminalConfigSchema.safeParse(config);
    expect(result.success).toBe(false);
  });

  it("rejects empty name", () => {
    const config = {
      id: "terminal-1",
      name: "",
    };
    const result = TerminalConfigSchema.safeParse(config);
    expect(result.success).toBe(false);
  });
});

describe("LoomConfigSchema", () => {
  it("validates a valid v2 config", () => {
    const config = {
      version: "2" as const,
      terminals: [
        { id: "terminal-1", name: "Builder" },
        { id: "terminal-2", name: "Judge" },
      ],
    };
    const result = LoomConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
  });

  it("validates config with offlineMode", () => {
    const config = {
      version: "2" as const,
      terminals: [],
      offlineMode: true,
    };
    const result = LoomConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.offlineMode).toBe(true);
    }
  });

  it("rejects wrong version", () => {
    const config = {
      version: "1",
      terminals: [],
    };
    const result = LoomConfigSchema.safeParse(config);
    expect(result.success).toBe(false);
  });

  it("rejects config without terminals array", () => {
    const config = {
      version: "2",
    };
    const result = LoomConfigSchema.safeParse(config);
    expect(result.success).toBe(false);
  });
});

describe("RawLoomConfigSchema", () => {
  it("accepts any version string for migration", () => {
    const config = {
      version: "1",
      terminals: [{ id: "t1", name: "test" }],
    };
    const result = RawLoomConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
  });

  it("accepts legacy agents array for detection", () => {
    const config = {
      agents: [{ id: "a1", name: "test" }],
    };
    const result = RawLoomConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.agents).toBeDefined();
    }
  });
});

describe("TerminalStateSchema", () => {
  it("validates a minimal terminal state", () => {
    const state = {
      id: "terminal-1",
      status: "idle",
      isPrimary: false,
    };
    const result = TerminalStateSchema.safeParse(state);
    expect(result.success).toBe(true);
  });

  it("validates all terminal statuses", () => {
    const statuses = ["idle", "busy", "needs_input", "error", "stopped"];
    for (const status of statuses) {
      const state = {
        id: "terminal-1",
        status,
        isPrimary: false,
      };
      const result = TerminalStateSchema.safeParse(state);
      expect(result.success).toBe(true);
    }
  });

  it("validates optional fields", () => {
    const state = {
      id: "terminal-1",
      status: "busy",
      isPrimary: true,
      worktreePath: "/path/to/worktree",
      agentPid: 12345,
      agentStatus: "ready",
      lastIntervalRun: Date.now(),
      pendingInputRequests: [{ id: "req-1", prompt: "Question?", timestamp: Date.now() }],
      busyTime: 60000,
      idleTime: 30000,
      lastStateChange: Date.now(),
    };
    const result = TerminalStateSchema.safeParse(state);
    expect(result.success).toBe(true);
  });

  it("rejects invalid status", () => {
    const state = {
      id: "terminal-1",
      status: "invalid_status",
      isPrimary: false,
    };
    const result = TerminalStateSchema.safeParse(state);
    expect(result.success).toBe(false);
  });
});

describe("LoomStateSchema", () => {
  it("validates a valid state", () => {
    const state = {
      nextAgentNumber: 3,
      terminals: [
        { id: "terminal-1", status: "idle", isPrimary: true },
        { id: "terminal-2", status: "busy", isPrimary: false },
      ],
    };
    const result = LoomStateSchema.safeParse(state);
    expect(result.success).toBe(true);
  });

  it("validates state with daemonPid", () => {
    const state = {
      daemonPid: 12345,
      nextAgentNumber: 1,
      terminals: [],
    };
    const result = LoomStateSchema.safeParse(state);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.daemonPid).toBe(12345);
    }
  });

  it("rejects nextAgentNumber less than 1", () => {
    const state = {
      nextAgentNumber: 0,
      terminals: [],
    };
    const result = LoomStateSchema.safeParse(state);
    expect(result.success).toBe(false);
  });
});

describe("GitIdentitySchema", () => {
  it("validates a valid git identity", () => {
    const identity = {
      name: "Builder Bot",
      email: "builder@example.com",
    };
    const result = GitIdentitySchema.safeParse(identity);
    expect(result.success).toBe(true);
  });

  it("rejects empty name", () => {
    const identity = {
      name: "",
      email: "builder@example.com",
    };
    const result = GitIdentitySchema.safeParse(identity);
    expect(result.success).toBe(false);
  });

  it("rejects invalid email", () => {
    const identity = {
      name: "Builder Bot",
      email: "not-an-email",
    };
    const result = GitIdentitySchema.safeParse(identity);
    expect(result.success).toBe(false);
  });
});

describe("RoleMetadataSchema", () => {
  it("validates a minimal role metadata", () => {
    const metadata = {};
    const result = RoleMetadataSchema.safeParse(metadata);
    expect(result.success).toBe(true);
  });

  it("validates a full role metadata", () => {
    const metadata = {
      name: "Builder",
      description: "Implements features",
      defaultInterval: 300000,
      defaultIntervalPrompt: "Continue working",
      autonomousRecommended: false,
      suggestedWorkerType: "claude",
      gitIdentity: {
        name: "Builder Bot",
        email: "builder@example.com",
      },
    };
    const result = RoleMetadataSchema.safeParse(metadata);
    expect(result.success).toBe(true);
  });

  it("validates all worker types", () => {
    const workerTypes = ["claude", "none", "github-copilot", "gemini", "deepseek", "grok"];
    for (const workerType of workerTypes) {
      const metadata = {
        suggestedWorkerType: workerType,
      };
      const result = RoleMetadataSchema.safeParse(metadata);
      expect(result.success).toBe(true);
    }
  });

  it("rejects invalid worker type", () => {
    const metadata = {
      suggestedWorkerType: "invalid-worker",
    };
    const result = RoleMetadataSchema.safeParse(metadata);
    expect(result.success).toBe(false);
  });

  it("rejects negative defaultInterval", () => {
    const metadata = {
      defaultInterval: -1000,
    };
    const result = RoleMetadataSchema.safeParse(metadata);
    expect(result.success).toBe(false);
  });
});

describe("ActivityEntrySchema", () => {
  it("validates a valid activity entry", () => {
    const entry = {
      inputId: 1,
      timestamp: new Date().toISOString(),
      inputType: "manual",
      prompt: "Build the feature",
      agentRole: "builder",
      gitBranch: "feature/test",
      outputPreview: "Building...",
      exitCode: 0,
      outputTimestamp: new Date().toISOString(),
    };
    const result = ActivityEntrySchema.safeParse(entry);
    expect(result.success).toBe(true);
  });

  it("validates all input types", () => {
    const inputTypes = ["manual", "autonomous", "system", "user_instruction"];
    for (const inputType of inputTypes) {
      const entry = {
        inputId: 1,
        timestamp: new Date().toISOString(),
        inputType,
        prompt: "Test",
        agentRole: null,
        gitBranch: null,
        outputPreview: null,
        exitCode: null,
        outputTimestamp: null,
      };
      const result = ActivityEntrySchema.safeParse(entry);
      expect(result.success).toBe(true);
    }
  });

  it("allows null values for optional fields", () => {
    const entry = {
      inputId: 1,
      timestamp: new Date().toISOString(),
      inputType: "manual",
      prompt: "Test",
      agentRole: null,
      gitBranch: null,
      outputPreview: null,
      exitCode: null,
      outputTimestamp: null,
    };
    const result = ActivityEntrySchema.safeParse(entry);
    expect(result.success).toBe(true);
  });
});
