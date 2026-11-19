/**
 * Terminal tools for Loom MCP server
 *
 * Provides tools for managing Loom terminal sessions:
 * - List, create, delete, restart terminals
 * - Send input to terminals
 * - Get terminal output
 * - Configure terminal settings
 * - Control autonomous mode
 * - Query agent metrics
 */

import { readFile } from "node:fs/promises";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import stripAnsi from "strip-ansi";
import { getWorkspacePath, readConfigFile, writeConfigFile } from "../shared/config.js";
import {
  LOCAL_CONFIG_REL,
  PROJECT_CONFIG_REL,
  hasNonLegacyTier,
} from "../shared/config-resolver.js";
import { sendDaemonRequest } from "../shared/daemon.js";
import { formatTerminalOutput } from "../shared/formatting.js";
import type {
  ConfigureTerminalOptions,
  CreateTerminalConfig,
  LogResult,
  Terminal,
} from "../types.js";
// `get_agent_metrics` is the one tool here that spawns a child process; its
// argument validation and execution live in their own module (Issue #9107).
import { getAgentMetrics } from "./agent-metrics.js";

/**
 * Extract error message from daemon error responses.
 * Handles both legacy Error and new StructuredError (Issue #1171) formats.
 */
function extractDaemonErrorMessage(response: {
  type: string;
  payload?: unknown;
  message?: string;
}): string | null {
  if (response.type === "Error") {
    return response.message || "Unknown error";
  }
  if (response.type === "StructuredError") {
    const payload = response.payload as { message?: string } | undefined;
    return payload?.message || "Unknown structured error";
  }
  return null;
}

/**
 * List all active terminals from the live `loom-daemon` registry.
 *
 * The daemon garbage-collects registry entries whose backing tmux session no
 * longer exists on every `ListTerminals` request (see
 * `TerminalManager::prune_dead_terminals` in `loom-daemon/src/terminal.rs`),
 * so every terminal returned here is tmux-verified.
 *
 * There is deliberately NO fallback source (Issue #13821). The previous
 * `.loom/state.json` fallback was written only by the Tauri desktop app
 * removed in #3353, so on a daemon-unreachable call it could only ever
 * return a stale snapshot or nothing at all — the unverified list #4794 had
 * to warn about. An unreachable daemon now rejects: the connection error
 * propagates to `handleTerminalTool`'s caller in `index.ts`, which reports it
 * as a tool error. That is the fail-closed answer — "the daemon did not
 * answer" must not be rendered as the indistinguishable "there are no
 * terminals".
 */
async function listTerminals(): Promise<Terminal[]> {
  const response = (await sendDaemonRequest({
    type: "ListTerminals",
  })) as { type: string; payload?: { terminals: Terminal[] } };

  if (response.type === "TerminalList" && response.payload?.terminals) {
    return response.payload.terminals;
  }

  return [];
}

/**
 * Get terminal output from the log file
 */
async function getTerminalOutput(terminalId: string, lines = 20): Promise<LogResult> {
  try {
    const logPath = `/tmp/loom-${terminalId}.out`;
    const content = await readFile(logPath, "utf-8");
    const allLines = content.split("\n");
    const totalLines = allLines.filter(Boolean).length;

    // Get last N lines (excluding empty trailing line)
    const relevantLines = allLines.slice(-lines - 1, -1).filter(Boolean);
    const linesReturned = relevantLines.length;

    // Strip ANSI escape sequences from output before returning
    const cleanOutput = relevantLines.map((line) => stripAnsi(line)).join("\n");

    return {
      content: cleanOutput,
      linesReturned,
      totalLines,
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return {
        content: "",
        linesReturned: 0,
        totalLines: 0,
        error: `Terminal output file not found for ${terminalId}.\n\nThis usually means:\n- The terminal hasn't been created yet, or\n- The terminal was closed`,
      };
    }
    return {
      content: "",
      linesReturned: 0,
      totalLines: 0,
      error: `Error reading terminal output: ${error}`,
    };
  }
}

/**
 * Send input to a terminal
 */
async function sendTerminalInput(terminalId: string, input: string): Promise<string> {
  try {
    const response = (await sendDaemonRequest({
      type: "SendInput",
      payload: {
        id: terminalId,
        data: input,
      },
    })) as { type: string };

    if (response.type === "Success") {
      return "Input sent successfully";
    }

    return `Unexpected response: ${response.type}`;
  } catch (error) {
    return `Error sending input: ${error}`;
  }
}

/**
 * Generate a unique terminal ID
 */
async function generateTerminalId(): Promise<string> {
  const terminals = await listTerminals();
  const existingIds = terminals.map((t) => t.id);

  let maxNum = 0;
  for (const id of existingIds) {
    const match = id.match(/^terminal-(\d+)$/);
    if (match) {
      const num = parseInt(match[1], 10);
      if (num > maxNum) {
        maxNum = num;
      }
    }
  }

  return `terminal-${maxNum + 1}`;
}

/**
 * Create a new terminal
 */
async function createTerminal(config: CreateTerminalConfig): Promise<{
  success: boolean;
  terminal_id?: string;
  error?: string;
}> {
  try {
    const terminalId = await generateTerminalId();
    const name = config.name || `${config.role || "default"}-${Date.now()}`;

    const response = (await sendDaemonRequest({
      type: "CreateTerminal",
      payload: {
        config_id: terminalId,
        name,
        working_dir: config.workingDir || null,
        role: config.role || null,
        instance_number: 0,
      },
    })) as { type: string; payload?: { id: string }; message?: string };

    if (response.type === "TerminalCreated" && response.payload?.id) {
      return {
        success: true,
        terminal_id: response.payload.id,
      };
    }

    const errorMsg = extractDaemonErrorMessage(response);
    if (errorMsg) {
      return { success: false, error: errorMsg };
    }

    return {
      success: false,
      error: `Unexpected response: ${response.type}`,
    };
  } catch (error) {
    return {
      success: false,
      error: `Error creating terminal: ${error}`,
    };
  }
}

/**
 * Delete a terminal
 */
async function deleteTerminal(terminalId: string): Promise<{
  success: boolean;
  error?: string;
}> {
  try {
    const response = (await sendDaemonRequest({
      type: "DestroyTerminal",
      payload: {
        id: terminalId,
      },
    })) as { type: string; message?: string };

    if (response.type === "Success") {
      return { success: true };
    }

    const errorMsg = extractDaemonErrorMessage(response);
    if (errorMsg) {
      return { success: false, error: errorMsg };
    }

    return {
      success: false,
      error: `Unexpected response: ${response.type}`,
    };
  } catch (error) {
    return {
      success: false,
      error: `Error deleting terminal: ${error}`,
    };
  }
}

/**
 * Restart a terminal
 */
async function restartTerminal(terminalId: string): Promise<{
  success: boolean;
  terminal_id?: string;
  error?: string;
}> {
  try {
    const terminals = await listTerminals();
    const terminal = terminals.find((t) => t.id === terminalId);

    if (!terminal) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found`,
      };
    }

    const savedConfig = {
      name: terminal.name,
      role: terminal.role,
      working_dir: terminal.working_dir,
    };

    const destroyResult = await deleteTerminal(terminalId);
    if (!destroyResult.success) {
      return {
        success: false,
        error: `Failed to destroy terminal: ${destroyResult.error}`,
      };
    }

    await new Promise((resolve) => setTimeout(resolve, 500));

    const response = (await sendDaemonRequest({
      type: "CreateTerminal",
      payload: {
        config_id: terminalId,
        name: savedConfig.name,
        working_dir: savedConfig.working_dir || null,
        role: savedConfig.role || null,
        instance_number: 0,
      },
    })) as { type: string; payload?: { id: string }; message?: string };

    if (response.type === "TerminalCreated" && response.payload?.id) {
      return {
        success: true,
        terminal_id: response.payload.id,
      };
    }

    const errorMsg = extractDaemonErrorMessage(response);
    if (errorMsg) {
      return { success: false, error: errorMsg };
    }

    return {
      success: false,
      error: `Unexpected response: ${response.type}`,
    };
  } catch (error) {
    return {
      success: false,
      error: `Error restarting terminal: ${error}`,
    };
  }
}

/**
 * Configure a terminal
 */
async function configureTerminal(
  terminalId: string,
  options: ConfigureTerminalOptions
): Promise<{
  success: boolean;
  error?: string;
}> {
  try {
    if (!options.name && !options.role && !options.roleConfig && !options.theme) {
      return {
        success: false,
        error: "At least one configuration option must be provided",
      };
    }

    const config = await readConfigFile();
    if (!config) {
      return {
        success: false,
        error: "Config file not found. Workspace may not be initialized.",
      };
    }

    // Write-policy guard (issue #4064): `readConfigFile` now returns the merged
    // view across config tiers. Writing that back to `.loom/config.json` when a
    // non-legacy tier is present would flatten every `.loom-project/` and
    // `.loom-local/` override into the legacy file, silently de-syncing it from
    // its source of truth. Since this resolver family is read-only (there is no
    // tier-aware write path), refuse the edit and direct the operator to the
    // tier file instead.
    if (await hasNonLegacyTier(getWorkspacePath())) {
      return {
        success: false,
        error:
          `Refusing to edit ${terminalId}: a non-legacy config tier is present ` +
          `(${PROJECT_CONFIG_REL} or ${LOCAL_CONFIG_REL}). mcp-loom reads the merged ` +
          `config across all tiers, but can only write the legacy .loom/config.json — ` +
          `writing here would flatten your tier overrides into it and de-sync them. ` +
          `Edit the terminal in the tier file that defines it directly.`,
      };
    }

    const terminalIndex = config.terminals.findIndex((t) => t.id === terminalId);
    if (terminalIndex === -1) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found in config`,
      };
    }

    const terminal = config.terminals[terminalIndex];

    if (options.name !== undefined) {
      terminal.name = options.name;
    }
    if (options.role !== undefined) {
      terminal.role = options.role;
    }
    if (options.theme !== undefined) {
      terminal.theme = options.theme;
    }
    if (options.roleConfig !== undefined) {
      terminal.roleConfig = {
        ...terminal.roleConfig,
        ...options.roleConfig,
      };
    }

    config.terminals[terminalIndex] = terminal;
    await writeConfigFile(config);

    return { success: true };
  } catch (error) {
    return {
      success: false,
      error: `Error configuring terminal: ${error}`,
    };
  }
}

/**
 * Terminal tool definitions
 *
 * Removed tools (Issue #13821): `set_primary_terminal` and
 * `get_selected_terminal` are gone. Both operated on a home-directory
 * `~/.loom/state.json` whose only writer was `set_primary_terminal` itself —
 * a closed loop left behind when #3353 removed the Tauri desktop app that
 * owned the "primary terminal" concept. No daemon-era equivalent exists:
 * the daemon registry has no notion of a primary or selected terminal.
 */
export const terminalTools: Tool[] = [
  {
    name: "list_terminals",
    description:
      "List all active Loom terminal sessions with their IDs, names, roles, and working directories. Use this to discover which terminals are available.",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },
  {
    name: "get_terminal_output",
    description:
      "Get the recent output from a specific terminal. Returns the last N lines of output (default 20). Use this to see what a terminal is currently showing.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID (e.g., 'terminal-1', 'terminal-2')",
        },
        lines: {
          type: "number",
          description: "Number of lines to show (default: 20)",
          default: 20,
        },
      },
      required: ["terminal_id"],
    },
  },
  {
    name: "send_terminal_input",
    description:
      "Send input (commands or text) to a specific terminal. Use this to execute commands in a terminal. Note: Input is sent as literal text, so include '\\n' for Enter key.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID (e.g., 'terminal-1')",
        },
        input: {
          type: "string",
          description: "Text or command to send. Use '\\n' to send Enter, '\\u0003' for Ctrl+C",
        },
      },
      required: ["terminal_id", "input"],
    },
  },
  {
    name: "create_terminal",
    description:
      "Create a new Loom terminal session. Returns the new terminal's ID which can be used for other operations. The terminal will be created with its own tmux session.",
    inputSchema: {
      type: "object",
      properties: {
        name: {
          type: "string",
          description: "Human-readable name for the terminal (e.g., 'Builder', 'Judge')",
        },
        role: {
          type: "string",
          description: "Role for the terminal (e.g., 'builder', 'judge', 'curator')",
        },
        role_file: {
          type: "string",
          description: "Role definition file (e.g., 'builder.md', 'judge.md')",
        },
        target_interval: {
          type: "number",
          description: "Interval in milliseconds for autonomous operation (0 for manual)",
          default: 0,
        },
        interval_prompt: {
          type: "string",
          description: "Prompt to send at each interval for autonomous terminals",
          default: "",
        },
        theme: {
          type: "string",
          description: "Terminal theme (optional)",
        },
        working_dir: {
          type: "string",
          description: "Working directory for the terminal (optional, defaults to workspace root)",
        },
      },
    },
  },
  {
    name: "delete_terminal",
    description:
      "Delete (destroy) a Loom terminal session. This will kill the tmux session, clean up the output log file, and remove any associated worktree if it's not used by other terminals.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID to delete (e.g., 'terminal-1')",
        },
      },
      required: ["terminal_id"],
    },
  },
  {
    name: "restart_terminal",
    description:
      "Restart a Loom terminal session. This preserves the terminal's configuration (ID, name, role, working directory) but creates a fresh tmux session. Useful for recovering from stuck states or clearing terminal history.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID to restart (e.g., 'terminal-1')",
        },
      },
      required: ["terminal_id"],
    },
  },
  {
    name: "configure_terminal",
    description:
      "Update a terminal's configuration settings. Changes are saved to the config file and take effect on next terminal restart or when the UI hot-reloads the configuration. Use this to change terminal name, role, theme, or autonomous interval settings.",
    inputSchema: {
      type: "object",
      properties: {
        terminal_id: {
          type: "string",
          description: "Terminal ID to configure (e.g., 'terminal-1')",
        },
        name: {
          type: "string",
          description: "New human-readable name for the terminal",
        },
        role: {
          type: "string",
          description: "New role for the terminal (e.g., 'claude-code-worker')",
        },
        role_config: {
          type: "object",
          description: "Role configuration settings",
          properties: {
            worker_type: {
              type: "string",
              description: "Worker type (e.g., 'claude')",
            },
            role_file: {
              type: "string",
              description: "Role definition file (e.g., 'builder.md', 'judge.md')",
            },
            target_interval: {
              type: "number",
              description: "Interval in milliseconds for autonomous operation (0 for manual)",
            },
            interval_prompt: {
              type: "string",
              description: "Prompt to send at each interval for autonomous terminals",
            },
          },
        },
        theme: {
          type: "string",
          description: "Terminal theme (e.g., 'ocean', 'forest', 'sunset')",
        },
      },
      required: ["terminal_id"],
    },
  },
  {
    // The `enum`s below are advertised to the client; they are NOT enforced by
    // the MCP SDK. The server-side allow-lists in `agent-metrics.ts` are what
    // actually gate these values (Issue #9107) — keep the two in sync.
    name: "get_agent_metrics",
    description:
      "Get agent performance metrics for self-aware behavior. Enables agents to query their own effectiveness, costs, and velocity. Use this to check if struggling with a task type, select approaches based on historical success, or decide to escalate when below threshold. Part of Phase 5 (Autonomous Learning).",
    inputSchema: {
      type: "object",
      properties: {
        command: {
          type: "string",
          enum: ["summary", "effectiveness", "costs", "velocity"],
          description:
            "Type of metrics to retrieve: summary (overall metrics), effectiveness (success rates by role), costs (cost breakdown by issue), velocity (development velocity trends)",
          default: "summary",
        },
        role: {
          type: "string",
          enum: [
            "builder",
            "judge",
            "curator",
            "architect",
            "hermit",
            "doctor",
            "guide",
            "champion",
            "shepherd",
          ],
          description: "Filter metrics by agent role (optional)",
        },
        period: {
          type: "string",
          enum: ["today", "week", "month", "all"],
          description: "Time period for metrics: today, week, month, or all (default: week)",
          default: "week",
        },
        format: {
          type: "string",
          enum: ["json", "text"],
          description: "Output format: json for programmatic use, text for human-readable",
          default: "json",
        },
        issue: {
          type: "number",
          description: "Filter costs by specific issue number (only for 'costs' command)",
        },
      },
    },
  },
];

/**
 * Handle terminal tool calls
 */
export async function handleTerminalTool(
  name: string,
  args?: Record<string, unknown>
): Promise<{ type: "text"; text: string }[]> {
  switch (name) {
    case "list_terminals": {
      // Daemon-only, and deliberately throwing when the daemon is unreachable
      // (Issue #13821): an empty list here means the daemon answered and the
      // registry is empty, never that nobody answered.
      const terminals = await listTerminals();

      if (terminals.length === 0) {
        return [
          {
            type: "text",
            text: "No active terminals found. Either Loom hasn't been started yet, or all terminals have been closed.",
          },
        ];
      }

      const terminalList = terminals
        .map((t) => {
          const parts = [
            `ID: ${t.id}`,
            `Name: ${t.name}`,
            t.role ? `Role: ${t.role}` : null,
            t.working_dir ? `Working Dir: ${t.working_dir}` : null,
            `Session: ${t.tmux_session}`,
          ]
            .filter(Boolean)
            .join("\n  ");
          return `* ${parts}`;
        })
        .join("\n\n");

      return [
        {
          type: "text",
          text: `=== Active Loom Terminals (${terminals.length}) ===\n\n${terminalList}`,
        },
      ];
    }

    case "get_terminal_output": {
      const terminalId = args?.terminal_id as string;
      const lines = (args?.lines as number) || 20;

      if (!terminalId) {
        return [{ type: "text", text: "Error: terminal_id is required" }];
      }

      const result = await getTerminalOutput(terminalId, lines);
      return [{ type: "text", text: formatTerminalOutput(result, terminalId) }];
    }

    case "send_terminal_input": {
      const terminalId = args?.terminal_id as string;
      const input = args?.input as string;

      if (!terminalId || !input) {
        return [{ type: "text", text: "Error: terminal_id and input are required" }];
      }

      const result = await sendTerminalInput(terminalId, input);
      return [{ type: "text", text: result }];
    }

    case "create_terminal": {
      const config: CreateTerminalConfig = {
        name: args?.name as string | undefined,
        role: args?.role as string | undefined,
        roleFile: args?.role_file as string | undefined,
        targetInterval: args?.target_interval as number | undefined,
        intervalPrompt: args?.interval_prompt as string | undefined,
        theme: args?.theme as string | undefined,
        workingDir: args?.working_dir as string | undefined,
      };

      const result = await createTerminal(config);

      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Create Terminal ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }

      const details = [
        `Terminal ID: ${result.terminal_id}`,
        config.name ? `Name: ${config.name}` : null,
        config.role ? `Role: ${config.role}` : null,
        config.roleFile ? `Role File: ${config.roleFile}` : null,
        config.workingDir ? `Working Dir: ${config.workingDir}` : null,
      ]
        .filter(Boolean)
        .join("\n");

      return [
        {
          type: "text",
          text: `=== Create Terminal ===\n\nSuccess\n\n${details}\n\nThe terminal has been created with its own tmux session. You can now send commands to it using send_terminal_input.`,
        },
      ];
    }

    case "delete_terminal": {
      const terminalId = args?.terminal_id as string;

      if (!terminalId) {
        return [{ type: "text", text: "Error: terminal_id is required" }];
      }

      const result = await deleteTerminal(terminalId);

      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Delete Terminal ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }

      return [
        {
          type: "text",
          text: `=== Delete Terminal ===\n\nSuccess\n\nTerminal ${terminalId} has been deleted.\n\nCleanup performed:\n- tmux session killed\n- Output log file removed\n- Worktree cleaned up (if not used by other terminals)`,
        },
      ];
    }

    case "restart_terminal": {
      const terminalId = args?.terminal_id as string;

      if (!terminalId) {
        return [{ type: "text", text: "Error: terminal_id is required" }];
      }

      const result = await restartTerminal(terminalId);

      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Restart Terminal ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }

      return [
        {
          type: "text",
          text: `=== Restart Terminal ===\n\nSuccess\n\nTerminal ${result.terminal_id} has been restarted.\n\nThe terminal's configuration (ID, name, role, working directory) was preserved, but a fresh tmux session was created.\n\nNote: Any running processes in the terminal were terminated and terminal history was cleared.`,
        },
      ];
    }

    case "configure_terminal": {
      const terminalId = args?.terminal_id as string;

      if (!terminalId) {
        return [{ type: "text", text: "Error: terminal_id is required" }];
      }

      const roleConfigArgs = args?.role_config as
        | {
            worker_type?: string;
            role_file?: string;
            target_interval?: number;
            interval_prompt?: string;
          }
        | undefined;

      const options: ConfigureTerminalOptions = {};

      if (args?.name !== undefined) {
        options.name = args.name as string;
      }
      if (args?.role !== undefined) {
        options.role = args.role as string;
      }
      if (args?.theme !== undefined) {
        options.theme = args.theme as string;
      }
      if (roleConfigArgs !== undefined) {
        options.roleConfig = {};
        if (roleConfigArgs.worker_type !== undefined) {
          options.roleConfig.workerType = roleConfigArgs.worker_type;
        }
        if (roleConfigArgs.role_file !== undefined) {
          options.roleConfig.roleFile = roleConfigArgs.role_file;
        }
        if (roleConfigArgs.target_interval !== undefined) {
          options.roleConfig.targetInterval = roleConfigArgs.target_interval;
        }
        if (roleConfigArgs.interval_prompt !== undefined) {
          options.roleConfig.intervalPrompt = roleConfigArgs.interval_prompt;
        }
      }

      const result = await configureTerminal(terminalId, options);

      if (!result.success) {
        return [
          {
            type: "text",
            text: `=== Configure Terminal ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }

      const changedFields = Object.keys(options)
        .map((key) => {
          if (key === "roleConfig" && options.roleConfig) {
            return Object.keys(options.roleConfig)
              .map((k) => `roleConfig.${k}`)
              .join(", ");
          }
          return key;
        })
        .join(", ");

      return [
        {
          type: "text",
          text: `=== Configure Terminal ===\n\nSuccess\n\nTerminal ${terminalId} configuration updated.\n\nUpdated fields: ${changedFields}\n\nNote: Changes are saved to the config file. The terminal may need to be restarted for some changes to take effect, or the UI will hot-reload the configuration.`,
        },
      ];
    }

    case "get_agent_metrics": {
      // Issue #9107: pass the raw MCP values straight through — no `as string`
      // casts, which asserted a type the SDK never checked. `getAgentMetrics`
      // allow-lists every one of them before anything is spawned, and echoes
      // the resolved values back in `result.filters` for display below.
      const result = await getAgentMetrics({
        command: args?.command,
        role: args?.role,
        period: args?.period,
        format: args?.format,
        issue: args?.issue,
      });

      if (!result.success || !result.filters) {
        return [
          {
            type: "text",
            text: `=== Agent Metrics ===\n\nFailed\n\n${result.error}`,
          },
        ];
      }

      const { command, role, period, format, issue } = result.filters;

      const header = `=== Agent Metrics (${command}) ===\n`;
      const filterInfo = [
        role ? `Role: ${role}` : null,
        `Period: ${period}`,
        issue ? `Issue: #${issue}` : null,
      ]
        .filter(Boolean)
        .join(" | ");

      const output =
        format === "json" && result.data ? JSON.stringify(result.data, null, 2) : result.output;

      return [
        {
          type: "text",
          text: `${header}${filterInfo ? `${filterInfo}\n\n` : "\n"}${output}`,
        },
      ];
    }

    default:
      throw new Error(`Unknown terminal tool: ${name}`);
  }
}
