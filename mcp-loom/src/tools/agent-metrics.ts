/**
 * `get_agent_metrics` argument validation and execution (Issue #9107).
 *
 * This lives in its own module, separate from the rest of the terminal tools,
 * because it is the one MCP tool in this server that spawns a child process —
 * the security-relevant surface deserves a file you can read end to end.
 *
 * The rule this module exists to enforce: the tool's `inputSchema` `enum`s are
 * advisory to the *client*. `@modelcontextprotocol/sdk`'s
 * `CallToolRequestSchema` handler (`src/index.ts`) hands `request.params.arguments`
 * to the tool as a bare `Record<string, unknown>` with no runtime schema check,
 * so a client that ignores the schema — or a model emitting an off-enum string
 * under prompt injection (see `defaults/docs/untrusted-external-content.md`) —
 * reaches the tool unfiltered. Server-side allow-lists here are the only
 * enforcement point.
 */

import { execFile } from "node:child_process";
import { stat } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { getWorkspacePath } from "../shared/config.js";
import type { AgentMetricsFilters, AgentMetricsResult } from "../types.js";

/**
 * Argument-array child spawn.
 *
 * Deliberately `execFile`, never `exec`: `exec` runs its single string argument
 * through `/bin/sh -c`, so any value interpolated into that string is shell
 * syntax. `execFile` hands an argv array straight to `execvp` with no shell in
 * between, which removes shell metacharacter injection as a category rather
 * than trying to escape it. Do not reintroduce `exec`/`shell: true` here —
 * `src/no-shell-string-construction.test.ts` fails the build if it comes back.
 */
const execFileAsync = promisify(execFile);

/**
 * Allow-listed `get_agent_metrics` argument values.
 *
 * These are the SAME sets the tool's `inputSchema` `enum`s declare in
 * `terminals.ts` — keep the two in sync. See the module header for why the
 * schema alone is not enough.
 */
export const AGENT_METRICS_COMMANDS = ["summary", "effectiveness", "costs", "velocity"] as const;
export const AGENT_METRICS_ROLES = [
  "builder",
  "judge",
  "curator",
  "architect",
  "hermit",
  "doctor",
  "guide",
  "champion",
  "shepherd",
] as const;
export const AGENT_METRICS_PERIODS = ["today", "week", "month", "all"] as const;
export const AGENT_METRICS_FORMATS = ["json", "text"] as const;

export type AgentMetricsCommand = (typeof AGENT_METRICS_COMMANDS)[number];
export type AgentMetricsRole = (typeof AGENT_METRICS_ROLES)[number];
export type AgentMetricsPeriod = (typeof AGENT_METRICS_PERIODS)[number];
export type AgentMetricsFormat = (typeof AGENT_METRICS_FORMATS)[number];

/** Raw, untrusted `get_agent_metrics` arguments as they arrive over MCP. */
export interface AgentMetricsOptions {
  command?: unknown;
  role?: unknown;
  period?: unknown;
  format?: unknown;
  issue?: unknown;
}

/**
 * A validated invocation: `argv` is safe to hand to `execFile` as-is, and the
 * echoed fields are the resolved (defaulted) values the caller may render.
 */
export interface AgentMetricsInvocation extends AgentMetricsFilters {
  command: AgentMetricsCommand;
  role?: AgentMetricsRole;
  period: AgentMetricsPeriod;
  format: AgentMetricsFormat;
}

export type AgentMetricsValidation =
  | { ok: true; invocation: AgentMetricsInvocation }
  | { ok: false; error: string };

/**
 * Render an off-allow-list value for an error message.
 *
 * Quoted (so a payload's whitespace and metacharacters are visible in the
 * message) and truncated (so a huge argument cannot flood the tool response).
 * This string is only ever returned to the caller as text — it is never
 * re-executed.
 */
function describeRejectedValue(value: unknown): string {
  let rendered: string;
  if (typeof value === "string") {
    rendered = JSON.stringify(value);
  } else if (value === null || value === undefined || typeof value !== "object") {
    // Numbers (including NaN, which JSON.stringify would render as `null`),
    // booleans, bigints, symbols, functions.
    rendered = String(value);
  } else {
    try {
      rendered = JSON.stringify(value) ?? String(value);
    } catch {
      rendered = "[unserializable value]";
    }
  }
  return rendered.length > 80 ? `${rendered.slice(0, 77)}...` : rendered;
}

function validateEnum<T extends string>(
  name: string,
  value: unknown,
  allowed: readonly T[]
): { ok: true; value: T } | { ok: false; error: string } {
  if (typeof value !== "string" || !(allowed as readonly string[]).includes(value)) {
    return {
      ok: false,
      error:
        `Invalid "${name}" argument: ${describeRejectedValue(value)}. ` +
        `Expected one of: ${allowed.join(", ")}.`,
    };
  }
  return { ok: true, value: value as T };
}

/**
 * Validate raw MCP arguments and build the `agent-metrics.sh` argv array
 * (Issue #9107).
 *
 * Fail-closed: any off-allow-list value returns `{ ok: false, error }` naming
 * the offending argument, and the caller spawns no child process. Absent
 * (`undefined`/`null`) optional values keep their historical defaults so a
 * valid call produces byte-identical argv to the pre-#9107 shell string:
 * `command` defaults to `summary` (and, matching the old builder, is omitted
 * from argv in that case — `agent-metrics.sh` prepends `summary` itself),
 * `period` to `week` and `format` to `json`. Anything *present* but not on the
 * allow-list — including an empty string, which the old code silently treated
 * as "no filter" — is an error rather than a guess.
 */
export function validateAgentMetricsOptions(options: AgentMetricsOptions): AgentMetricsValidation {
  const commandResult = validateEnum(
    "command",
    options.command ?? "summary",
    AGENT_METRICS_COMMANDS
  );
  if (!commandResult.ok) {
    return { ok: false, error: commandResult.error };
  }

  let role: AgentMetricsRole | undefined;
  if (options.role !== undefined && options.role !== null) {
    const roleResult = validateEnum("role", options.role, AGENT_METRICS_ROLES);
    if (!roleResult.ok) {
      return { ok: false, error: roleResult.error };
    }
    role = roleResult.value;
  }

  const periodResult = validateEnum("period", options.period ?? "week", AGENT_METRICS_PERIODS);
  if (!periodResult.ok) {
    return { ok: false, error: periodResult.error };
  }

  const formatResult = validateEnum("format", options.format ?? "json", AGENT_METRICS_FORMATS);
  if (!formatResult.ok) {
    return { ok: false, error: formatResult.error };
  }

  let issue: number | undefined;
  if (options.issue !== undefined && options.issue !== null) {
    const candidate = options.issue;
    if (typeof candidate !== "number" || !Number.isInteger(candidate) || candidate < 1) {
      return {
        ok: false,
        error:
          `Invalid "issue" argument: ${describeRejectedValue(candidate)}. ` +
          `Expected an integer issue number >= 1.`,
      };
    }
    issue = candidate;
  }

  const command = commandResult.value;
  const period = periodResult.value;
  const format = formatResult.value;

  // Each element is pushed individually — never joined into one string, so no
  // value can ever be re-split or reinterpreted downstream.
  const argv: string[] = [];
  if (command !== "summary") {
    argv.push(command);
  }
  if (role) {
    argv.push("--role", role);
  }
  argv.push("--period", period);
  argv.push("--format", format);
  if (issue !== undefined) {
    argv.push("--issue", String(issue));
  }

  return { ok: true, invocation: { argv, command, role, period, format, issue } };
}

/**
 * Get agent performance metrics by running `.loom/scripts/agent-metrics.sh`.
 *
 * Takes raw, untrusted MCP arguments — validation happens here, not at the
 * call site, so no caller can forget it.
 */
export async function getAgentMetrics(options: AgentMetricsOptions): Promise<AgentMetricsResult> {
  // Validate BEFORE touching the filesystem or spawning anything: an invalid
  // argument must never reach a child process.
  const validation = validateAgentMetricsOptions(options);
  if (!validation.ok) {
    return {
      success: false,
      error: validation.error,
      format: "text",
      output: "",
    };
  }

  // `filters` is echoed back in the result so callers render the resolved
  // values the run actually used instead of re-casting the raw `unknown`s.
  const filters = validation.invocation;
  const { argv, format } = filters;
  const workspacePath = getWorkspacePath();
  const scriptPath = join(workspacePath, ".loom", "scripts", "agent-metrics.sh");

  try {
    await stat(scriptPath);
  } catch {
    return {
      success: false,
      error: `Agent metrics script not found at ${scriptPath}. Ensure Loom is installed.`,
      format: "text",
      output: "",
    };
  }

  try {
    const { stdout, stderr } = await execFileAsync("bash", [scriptPath, ...argv], {
      cwd: workspacePath,
    });

    if (stderr) {
      console.error("agent-metrics.sh stderr:", stderr);
    }

    let data: unknown;
    if (format === "json") {
      try {
        data = JSON.parse(stdout.trim());
      } catch {
        return {
          success: true,
          output: stdout.trim(),
          format: "text",
          filters,
        };
      }
    }

    return {
      success: true,
      data,
      output: stdout.trim(),
      format,
      filters,
    };
  } catch (error) {
    const err = error as { stderr?: string; message?: string };
    return {
      success: false,
      error: err.stderr || err.message || String(error),
      format: "text",
      output: "",
    };
  }
}
