/**
 * Tests for `get_agent_metrics` argument validation and its argv-array child
 * spawn (Issue #9107).
 *
 * Before this, the tool built its command line by interpolating raw MCP tool
 * arguments into a template literal and handing the result to
 * `child_process.exec` — i.e. to `/bin/sh -c`. The `inputSchema` `enum`
 * constraints are advisory to the client (the MCP SDK does not enforce them
 * server-side), so `role: "x; touch /tmp/mcp-loom-pwn #"` was arbitrary command
 * execution as the user running the server. Verified against the pre-fix call
 * shape while writing these tests: that exact payload created the file.
 *
 * Two independent defences are pinned here: allow-list validation (nothing
 * invalid is ever spawned) and the argv array (no shell exists to interpret a
 * metacharacter even if validation were bypassed).
 */

import { access, mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  AGENT_METRICS_COMMANDS,
  AGENT_METRICS_FORMATS,
  AGENT_METRICS_PERIODS,
  AGENT_METRICS_ROLES,
  getAgentMetrics,
  validateAgentMetricsOptions,
} from "./agent-metrics.js";
import { handleTerminalTool, terminalTools } from "./terminals.js";

/**
 * Reference implementation of the pre-#9107 argv builder, transcribed from the
 * code this issue replaced (with the defaults the `get_agent_metrics` handler
 * applied before calling it). The old call site was
 * `exec("bash \"${scriptPath}\" " + args.join(" "))`, so for values that
 * contain no shell metacharacters — i.e. every allow-listed value — the shell's
 * word splitting produced exactly `["bash", scriptPath, ...args]`. Comparing
 * the new argv against this is acceptance criterion 4: identical behaviour for
 * valid inputs.
 */
function legacyAgentMetricsArgs(options: {
  command?: string;
  role?: string;
  period?: string;
  format?: string;
  issue?: number;
}): string[] {
  const args: string[] = [];
  const command = options.command || "summary";
  if (command && command !== "summary") {
    args.push(command);
  }
  if (options.role) {
    args.push("--role", options.role);
  }
  const period = options.period || "week";
  if (period) {
    args.push("--period", period);
  }
  const format = options.format || "json";
  args.push("--format", format);
  if (options.issue) {
    args.push("--issue", String(options.issue));
  }
  return args;
}

describe("validateAgentMetricsOptions (#9107)", () => {
  it("builds argv identical to the legacy shell string for every valid combination", () => {
    const roles: (string | undefined)[] = [undefined, ...AGENT_METRICS_ROLES];
    const issues: (number | undefined)[] = [undefined, 1, 42];
    let combinations = 0;

    for (const command of AGENT_METRICS_COMMANDS) {
      for (const role of roles) {
        for (const period of AGENT_METRICS_PERIODS) {
          for (const format of AGENT_METRICS_FORMATS) {
            for (const issue of issues) {
              const result = validateAgentMetricsOptions({ command, role, period, format, issue });
              expect(result.ok).toBe(true);
              if (!result.ok) {
                return;
              }
              expect(result.invocation.argv).toEqual(
                legacyAgentMetricsArgs({ command, role, period, format, issue })
              );
              combinations += 1;
            }
          }
        }
      }
    }

    // 4 commands x 10 role values x 4 periods x 2 formats x 3 issue values.
    expect(combinations).toBe(960);
  });

  it("applies the historical defaults when optional arguments are absent", () => {
    const result = validateAgentMetricsOptions({});
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    // `summary` is omitted from argv exactly as before -- agent-metrics.sh
    // prepends it itself when no command word is given.
    expect(result.invocation.argv).toEqual(["--period", "week", "--format", "json"]);
    expect(result.invocation.command).toBe("summary");
    expect(result.invocation.period).toBe("week");
    expect(result.invocation.format).toBe("json");
    expect(result.invocation.role).toBeUndefined();
    expect(result.invocation.issue).toBeUndefined();
  });

  it("never emits an argv element containing a shell metacharacter", () => {
    const result = validateAgentMetricsOptions({
      command: "costs",
      role: "builder",
      period: "month",
      format: "text",
      issue: 9107,
    });
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    for (const element of result.invocation.argv) {
      expect(element).toMatch(/^[A-Za-z0-9_.=-]+$/);
    }
  });

  // Acceptance criterion 3: one rejection case per option. Each hostile value
  // is the kind of string that broke out of the old `${args.join(" ")}` shell
  // string.
  const REJECTED: { option: string; options: Record<string, unknown> }[] = [
    { option: "command", options: { command: "summary; touch pwned" } },
    { option: "command", options: { command: "$(touch pwned)" } },
    { option: "command", options: { command: "" } },
    { option: "command", options: { command: 7 } },
    { option: "role", options: { role: "x; touch /tmp/mcp-loom-pwn#" } },
    { option: "role", options: { role: "builder --format text" } },
    { option: "role", options: { role: "builder\ntouch pwned" } },
    { option: "role", options: { role: "`touch pwned`" } },
    { option: "role", options: { role: ["builder"] } },
    { option: "period", options: { period: "week && touch pwned" } },
    { option: "period", options: { period: "decade" } },
    { option: "format", options: { format: "json | touch pwned" } },
    { option: "format", options: { format: "yaml" } },
    { option: "issue", options: { issue: "42; touch pwned" } },
    { option: "issue", options: { issue: 0 } },
    { option: "issue", options: { issue: -1 } },
    { option: "issue", options: { issue: 1.5 } },
    { option: "issue", options: { issue: Number.NaN } },
  ];

  const describeValue = (value: unknown): string =>
    typeof value === "number" ? String(value) : JSON.stringify(value);

  for (const { option, options } of REJECTED) {
    it(`rejects ${option}=${describeValue(options[option])}`, () => {
      const result = validateAgentMetricsOptions(options);
      expect(result.ok).toBe(false);
      if (result.ok) {
        return;
      }
      expect(result.error).toContain(`"${option}"`);
    });
  }

  it("accepts a legitimately absent optional filter (null and undefined alike)", () => {
    for (const absent of [undefined, null]) {
      const result = validateAgentMetricsOptions({ role: absent, issue: absent });
      expect(result.ok).toBe(true);
    }
  });
});

/**
 * Integration tests against a real child process (Issue #9107, acceptance
 * criteria 2 and 3).
 *
 * `LOOM_WORKSPACE` points at a throwaway directory holding a stub
 * `.loom/scripts/agent-metrics.sh` that records the argv it received. The stub
 * running at all is the proof that a child was spawned, so its ABSENCE is the
 * proof that a rejected argument spawned nothing.
 */
describe("getAgentMetrics child process (#9107)", () => {
  let workspace: string;
  let previousWorkspace: string | undefined;

  const ARGV_CANARY = "argv-canary.txt";
  const PWN_CANARY = "mcp-loom-pwn";

  beforeEach(async () => {
    workspace = await mkdtemp(join(tmpdir(), "mcp-loom-9107-"));
    await mkdir(join(workspace, ".loom", "scripts"), { recursive: true });
    await writeFile(
      join(workspace, ".loom", "scripts", "agent-metrics.sh"),
      [
        "#!/bin/bash",
        "# Test stub: record the argv we were handed, then emit parseable JSON.",
        'printf "%s\\n" "$@" > "$LOOM_ARGV_CANARY"',
        "echo '{\"stub\": true}'",
        "",
      ].join("\n"),
      { mode: 0o755 }
    );
    previousWorkspace = process.env.LOOM_WORKSPACE;
    process.env.LOOM_WORKSPACE = workspace;
    process.env.LOOM_ARGV_CANARY = join(workspace, ARGV_CANARY);
  });

  afterEach(async () => {
    if (previousWorkspace === undefined) {
      delete process.env.LOOM_WORKSPACE;
    } else {
      process.env.LOOM_WORKSPACE = previousWorkspace;
    }
    delete process.env.LOOM_ARGV_CANARY;
    await rm(workspace, { recursive: true, force: true });
  });

  async function exists(path: string): Promise<boolean> {
    try {
      await access(path);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Everything in the workspace root. Asserting on the whole listing (rather
   * than on one expected filename) keeps the "no side effects" checks
   * non-vacuous whatever a payload happens to name its output: a shell would
   * have created *something* here, since the child's cwd is the workspace.
   *
   * Verified manually against the pre-#9107 code shape
   * (`exec("bash \"<script>\" " + args.join(" "))`) with the same payload: the
   * injected `touch` ran and the file appeared in this listing.
   */
  async function workspaceEntries(): Promise<string[]> {
    return (await readdir(workspace)).sort();
  }

  async function recordedArgv(): Promise<string[]> {
    const contents = await readFile(join(workspace, ARGV_CANARY), "utf-8");
    return contents.split("\n").filter((line) => line !== "");
  }

  it("passes validated arguments to the script as separate argv elements", async () => {
    const result = await getAgentMetrics({
      command: "costs",
      role: "builder",
      period: "month",
      format: "json",
      issue: 9107,
    });

    expect(result.success).toBe(true);
    expect(result.data).toEqual({ stub: true });
    expect(await recordedArgv()).toEqual([
      "costs",
      "--role",
      "builder",
      "--period",
      "month",
      "--format",
      "json",
      "--issue",
      "9107",
    ]);
  });

  it("returns a structured error and spawns nothing for an injected role", async () => {
    // The issue's own payload shape: a command separator, a side effect, and a
    // comment to swallow the trailing flags. Confirmed to execute under the
    // pre-#9107 `exec` + string-join call shape.
    const payload = `x; touch ${join(workspace, PWN_CANARY)} #`;

    const result = await getAgentMetrics({ role: payload });

    expect(result.success).toBe(false);
    expect(result.error).toContain('"role"');
    expect(result.output).toBe("");
    // No child process: the stub never ran, so it never wrote its canary.
    expect(await exists(join(workspace, ARGV_CANARY))).toBe(false);
    // And the injected command never executed -- nothing at all was created.
    expect(await workspaceEntries()).toEqual([".loom"]);
  });

  it("spawns nothing for an off-allow-list value in any single option", async () => {
    const payload = (prefix: string) => `${prefix}; touch ${join(workspace, PWN_CANARY)} #`;
    const hostile: { option: string; options: Record<string, unknown> }[] = [
      { option: "command", options: { command: payload("summary") } },
      { option: "role", options: { role: payload("builder") } },
      { option: "period", options: { period: payload("week") } },
      { option: "format", options: { format: payload("json") } },
      { option: "issue", options: { issue: payload("1") } },
    ];

    for (const { option, options } of hostile) {
      const result = await getAgentMetrics(options);
      expect(result.success, option).toBe(false);
      expect(result.error, option).toContain(`"${option}"`);
      expect(await exists(join(workspace, ARGV_CANARY))).toBe(false);
      expect(await workspaceEntries()).toEqual([".loom"]);
    }
  });

  it("surfaces the validation error through the MCP tool handler", async () => {
    const content = await handleTerminalTool("get_agent_metrics", {
      role: `x; touch ${join(workspace, PWN_CANARY)} #`,
    });

    expect(content).toHaveLength(1);
    expect(content[0].text).toContain("Failed");
    expect(content[0].text).toContain('"role"');
    expect(await exists(join(workspace, ARGV_CANARY))).toBe(false);
    expect(await workspaceEntries()).toEqual([".loom"]);
  });

  it("still serves a valid call through the MCP tool handler", async () => {
    const content = await handleTerminalTool("get_agent_metrics", {
      command: "effectiveness",
      role: "judge",
    });

    expect(content[0].text).toContain("=== Agent Metrics (effectiveness) ===");
    expect(content[0].text).toContain("Role: judge");
    expect(content[0].text).toContain("Period: week");
    expect(await recordedArgv()).toEqual([
      "effectiveness",
      "--role",
      "judge",
      "--period",
      "week",
      "--format",
      "json",
    ]);
  });
});

/**
 * The advertised `inputSchema` enums and the enforced allow-lists are two
 * copies of the same list, in two files. This pins them together so a future
 * edit to one is not silently ignored by the other — the failure mode being
 * either a valid documented value that the validator now rejects, or (worse) a
 * value the schema no longer advertises that the validator still accepts.
 */
describe("get_agent_metrics inputSchema matches the enforced allow-lists (#9107)", () => {
  const schema = terminalTools.find((tool) => tool.name === "get_agent_metrics")?.inputSchema as
    | {
        properties?: Record<string, { enum?: string[]; type?: string }>;
      }
    | undefined;

  it("declares the tool with per-option properties", () => {
    expect(schema?.properties).toBeDefined();
  });

  it.each([
    ["command", AGENT_METRICS_COMMANDS],
    ["role", AGENT_METRICS_ROLES],
    ["period", AGENT_METRICS_PERIODS],
    ["format", AGENT_METRICS_FORMATS],
  ] as const)("%s enum equals the allow-list", (option, allowed) => {
    expect(schema?.properties?.[option]?.enum).toEqual([...allowed]);
  });

  it("declares issue as a number", () => {
    expect(schema?.properties?.issue?.type).toBe("number");
  });
});
