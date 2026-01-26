/**
 * terminal-state-parser.test.ts - Tests for passive terminal state detection
 */

import { describe, expect, it } from "vitest";
import { parseTerminalState } from "./terminal-state-parser";

describe("parseTerminalState", () => {
  describe("Claude Code bypass permissions prompt", () => {
    it("should detect bypass permissions warning", () => {
      const output = `
WARNING: Claude Code running in Bypass Permissions mode...

Choose an option:
1) Continue with restricted permissions
2) Accept bypass permissions

Select option:
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("bypass-prompt");
      expect(state.raw).toBe(output);
    });

    it("should detect bypass warning with lowercase", () => {
      const output = "warning: Claude Code running in bypass permissions mode";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("bypass-prompt");
    });
  });

  describe("Claude Code ready state", () => {
    it("should detect ready state with ⏺ symbol", () => {
      const output = "⏺ How can I help you today?";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("waiting-input");
      expect(state.lastPrompt).toBe("⏺ How can I help you today?");
    });

    it("should detect ready state in multi-line output", () => {
      const output = `
[Previous output...]
Task completed successfully.

⏺ What would you like to do next?
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("waiting-input");
      expect(state.lastPrompt).toContain("⏺");
    });
  });

  describe("Claude Code paused state", () => {
    it("should detect paused state with ⏸ symbol", () => {
      const output = "⏸ Agent paused";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("paused");
    });
  });

  describe("Claude Code working state", () => {
    it("should detect working state with 'I'll help' pattern", () => {
      const output = "I'll help you implement that feature.";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });

    it("should detect working state with 'Let me' pattern", () => {
      const output = `
Let me analyze the codebase first.

Looking at the file structure...
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });

    it("should detect working state with function_calls", () => {
      const output = `
<function_calls>
<invoke name="Read">
<parameter name="file_path">/path/to/file</parameter>
</invoke>
</function_calls>
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });

    it("should detect working state with analyzing pattern", () => {
      const output = "Analyzing the requirements for this feature...";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });

    it("should detect working state with implementing pattern", () => {
      const output = "Implementing the requested changes now.";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });
  });

  describe("Shell prompts", () => {
    it("should detect bash prompt ($)", () => {
      const output = `
Last login: Mon Jan 01 12:00:00 on ttys001
$ `;
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect zsh prompt (%)", () => {
      const output = "% ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect root prompt (#)", () => {
      const output = "root@localhost:~# ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect bash version prompt", () => {
      const output = "bash-5.2$ ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect user@host prompt", () => {
      const output = "user@hostname:~/project$ ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect prompt after command output", () => {
      const output = `
$ ls -la
total 0
drwxr-xr-x  2 user user 64 Jan  1 12:00 .
drwxr-xr-x  3 user user 96 Jan  1 12:00 ..
$ `;
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });
  });

  describe("Codex detection", () => {
    it("should detect Codex from output marker", () => {
      const output = "[Codex] Analyzing your request...";
      const state = parseTerminalState(output);

      expect(state.type).toBe("codex");
      expect(state.status).toBe("working");
    });

    it("should detect Codex waiting for input", () => {
      const output = `
[Codex] Ready to assist.
> `;
      const state = parseTerminalState(output);

      expect(state.type).toBe("codex");
      expect(state.status).toBe("waiting-input");
    });
  });

  describe("Empty and minimal output", () => {
    it("should treat empty output as shell", () => {
      const output = "";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should treat whitespace-only output as shell", () => {
      const output = "   \n  \n  ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should treat very short output as shell", () => {
      const output = "ok";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });
  });

  describe("Unknown states", () => {
    it("should return unknown for ambiguous output", () => {
      const output = "Some random text that doesn't match any patterns";
      const state = parseTerminalState(output);

      expect(state.type).toBe("unknown");
      expect(state.status).toBe("unknown");
    });

    it("should return unknown for partial matches", () => {
      const output = "This might be an agent but no clear patterns";
      const state = parseTerminalState(output);

      expect(state.type).toBe("unknown");
      expect(state.status).toBe("unknown");
    });
  });

  describe("Priority ordering", () => {
    it("bypass prompt should take priority over working patterns", () => {
      const output = `
I'll help you with that.
WARNING: Claude Code running in Bypass Permissions mode
Let me get started.
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("bypass-prompt");
    });

    it("ready state should take priority over working patterns", () => {
      const output = `
I'm working on this task.
⏺ Ready for next command
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("waiting-input");
    });

    it("paused should take priority over working patterns", () => {
      const output = `
I was implementing this feature.
⏸ Paused by user
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("paused");
    });
  });

  describe("Edge cases", () => {
    it("should handle output with ANSI escape codes", () => {
      const output = "\x1b[32m$\x1b[0m ";
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should handle multi-line prompts", () => {
      const output = `
╭─ user@host ~/project
╰─$ `;
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should handle Unicode characters in output", () => {
      const output = "⏺ Hello 世界! How can I help?";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("waiting-input");
    });
  });

  describe("Claude wrapper states", () => {
    it("should detect wrapper starting", () => {
      const output = `
[2026-01-24 10:00:00] [INFO] Claude wrapper starting
[2026-01-24 10:00:00] [INFO] Arguments: --dangerously-skip-permissions
[2026-01-24 10:00:00] [INFO] Running pre-flight checks...
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-starting");
    });

    it("should detect wrapper pre-flight checks", () => {
      const output = `
[INFO] Running pre-flight checks...
[INFO] Claude CLI found: /usr/local/bin/claude
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-starting");
    });

    it("should detect wrapper in retry/backoff mode", () => {
      const output = `
[2026-01-24 10:05:00] [WARN] Claude CLI exited with code 1
[2026-01-24 10:05:00] [INFO] Detected transient error pattern: No messages returned
[2026-01-24 10:05:00] [WARN] Transient error detected. Waiting 2m 0s before retry...
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-retrying");
    });

    it("should detect wrapper max retries exceeded", () => {
      const output = `
[2026-01-24 10:15:00] [WARN] Transient error detected. Waiting 30m 0s before retry...
[2026-01-24 10:45:00] [ERROR] Max retries (5) exceeded
[2026-01-24 10:45:00] [ERROR] Last error: No messages returned from API
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-failed");
    });

    it("wrapper states should take priority over Claude working patterns", () => {
      const output = `
I'll help you with that.
[INFO] Running pre-flight checks...
Let me get started.
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-starting");
    });

    it("should detect wrapper retrying with different backoff messages", () => {
      const output = "[WARN] Waiting 1m 0s before retry...";
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("wrapper-retrying");
    });
  });

  describe("Real-world scenarios", () => {
    it("should detect Claude Code after agent launch", () => {
      const output = `
$ claude --dangerously-skip-permissions
[Claude Code initializing...]

⏺ I'm ready to help you. What would you like to work on?
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("waiting-input");
    });

    it("should detect shell after command execution", () => {
      const output = `
$ pnpm install
Progress: resolved 1234, reused 1000, downloaded 234
Dependencies installed successfully

$ `;
      const state = parseTerminalState(output);

      expect(state.type).toBe("shell");
      expect(state.status).toBe("idle");
    });

    it("should detect Claude working on a task", () => {
      const output = `
⏺ What would you like to do?
> Implement a new feature

Let me implement that feature for you. I'll start by analyzing the current codebase structure.

<function_calls>
<invoke name="Glob">
<parameter name="pattern">**/*.ts</parameter>
</invoke>
</function_calls>
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("working");
    });

    it("should detect bypass prompt during agent launch", () => {
      const output = `
$ claude --dangerously-skip-permissions

WARNING: Claude Code running in Bypass Permissions mode

This mode allows Claude to:
- Read and write files
- Execute shell commands
- Access the internet

Choose an option:
1) Continue with restricted permissions
2) Accept bypass permissions (recommended for development)

Select option (1-2):
`;
      const state = parseTerminalState(output);

      expect(state.type).toBe("claude-code");
      expect(state.status).toBe("bypass-prompt");
    });
  });
});
