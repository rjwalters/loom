#!/usr/bin/env node

import { exec } from "node:child_process";
import { mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import stripAnsi from "strip-ansi";

const execAsync = promisify(exec);

const SOCKET_PATH = process.env.LOOM_SOCKET_PATH || join(homedir(), ".loom", "loom-daemon.sock");
const LOOM_DIR = join(homedir(), ".loom");

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
const STATE_FILE = join(LOOM_DIR, "state.json");
const MCP_COMMAND_FILE = join(LOOM_DIR, "mcp-command.json");
const MCP_ACK_FILE = join(LOOM_DIR, "mcp-ack.json");

/**
 * Configuration for creating a terminal
 */
interface CreateTerminalConfig {
  name?: string;
  role?: string;
  roleFile?: string;
  targetInterval?: number;
  intervalPrompt?: string;
  theme?: string;
  workingDir?: string;
}

interface Terminal {
  id: string;
  name: string;
  role?: string;
  working_dir?: string;
  tmux_session: string;
  created_at: number;
  isPrimary?: boolean;
}

interface StateFile {
  terminals: Terminal[];
  selectedTerminalId: string | null;
  lastUpdated: string;
}

/**
 * Role configuration for a terminal
 */
interface RoleConfig {
  workerType?: string;
  roleFile?: string;
  targetInterval?: number;
  intervalPrompt?: string;
}

/**
 * Terminal configuration in config.json
 */
interface TerminalConfig {
  id: string;
  name: string;
  role?: string;
  roleConfig?: RoleConfig;
  theme?: string;
}

/**
 * Workspace config file structure
 */
interface ConfigFile {
  version: string;
  offlineMode?: boolean;
  terminals: TerminalConfig[];
}

/**
 * Send a request to the Loom daemon and get the response
 */
async function sendDaemonRequest(request: unknown): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const socket = new Socket();
    let buffer = "";

    socket.on("data", (data) => {
      buffer += data.toString();
    });

    socket.on("end", () => {
      try {
        const response = JSON.parse(buffer);
        resolve(response);
      } catch (error) {
        reject(new Error(`Failed to parse daemon response: ${error}`));
      }
    });

    socket.on("error", (error) => {
      reject(new Error(`Failed to connect to Loom daemon at ${SOCKET_PATH}: ${error.message}`));
    });

    socket.connect(SOCKET_PATH, () => {
      socket.write(JSON.stringify(request));
      socket.write("\n");
    });
  });
}

/**
 * Get the workspace path from environment or state file
 */
function getWorkspacePath(): string {
  return process.env.LOOM_WORKSPACE || join(homedir(), "GitHub", "loom");
}

/**
 * Read the state file to get terminal information
 */
async function readStateFile(): Promise<StateFile | null> {
  try {
    const fileStats = await stat(STATE_FILE);
    if (!fileStats.isFile()) {
      return null;
    }

    const content = await readFile(STATE_FILE, "utf-8");
    return JSON.parse(content) as StateFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * Write the state file
 */
async function writeStateFile(state: StateFile): Promise<void> {
  state.lastUpdated = new Date().toISOString();
  await writeFile(STATE_FILE, JSON.stringify(state, null, 2), "utf-8");
}

/**
 * Read the config file from the workspace
 */
async function readConfigFile(): Promise<ConfigFile | null> {
  try {
    const workspacePath = getWorkspacePath();
    const configPath = join(workspacePath, ".loom", "config.json");

    const fileStats = await stat(configPath);
    if (!fileStats.isFile()) {
      return null;
    }

    const content = await readFile(configPath, "utf-8");
    return JSON.parse(content) as ConfigFile;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

/**
 * Write the config file to the workspace
 */
async function writeConfigFile(config: ConfigFile): Promise<void> {
  const workspacePath = getWorkspacePath();
  const configPath = join(workspacePath, ".loom", "config.json");
  await writeFile(configPath, JSON.stringify(config, null, 2), "utf-8");
}

/**
 * List all active terminals from the daemon
 */
async function listTerminals(): Promise<Terminal[]> {
  try {
    const response = (await sendDaemonRequest({
      type: "ListTerminals",
    })) as { type: string; payload?: { terminals: Terminal[] } };

    if (response.type === "TerminalList" && response.payload?.terminals) {
      return response.payload.terminals;
    }

    return [];
  } catch (_error) {
    // If daemon is not running, fall back to state file
    const state = await readStateFile();
    return state?.terminals || [];
  }
}

/**
 * Result type for terminal output
 */
interface TerminalOutputResult {
  content: string;
  linesReturned: number;
  totalLines: number;
  error?: string;
}

/**
 * Format terminal output with line count metadata header
 */
function formatTerminalOutput(result: TerminalOutputResult, terminalId: string): string {
  if (result.error) {
    return `--- Terminal ${terminalId} Output (0 lines, file empty or does not exist) ---\n${result.error}`;
  }
  if (result.linesReturned === 0) {
    return `--- Terminal ${terminalId} Output (0 lines, file empty) ---\n(empty terminal output)`;
  }
  return `--- Terminal ${terminalId} Output (${result.linesReturned} lines returned, ${result.totalLines} total lines available) ---\n${result.content}`;
}

/**
 * Get terminal output from the log file
 */
async function getTerminalOutput(terminalId: string, lines = 20): Promise<TerminalOutputResult> {
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
 * Send input to a terminal (future feature)
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
 * Check tmux server health and count active loom sessions
 */
async function checkTmuxServerHealth(): Promise<{
  serverRunning: boolean;
  sessionCount: number;
  sessions: string[];
  errorMessage?: string;
}> {
  try {
    const { stdout } = await execAsync("tmux -L loom list-sessions -F '#{session_name}'");
    const sessions = stdout
      .trim()
      .split("\n")
      .filter((s) => s.startsWith("loom-"));

    return {
      serverRunning: true,
      sessionCount: sessions.length,
      sessions,
    };
  } catch (error: unknown) {
    const err = error as { code?: number; stderr?: string };
    return {
      serverRunning: false,
      sessionCount: 0,
      sessions: [],
      errorMessage: err.stderr || String(error),
    };
  }
}

/**
 * Get tmux server information (PID, socket path, version)
 */
async function getTmuxServerInfo(): Promise<{
  serverProcess?: string;
  socketPath: string;
  socketExists: boolean;
  tmuxVersion?: string;
  errorMessage?: string;
}> {
  const uid = process.getuid?.() || 0;
  const socketPath = `/private/tmp/tmux-${uid}/loom`;

  try {
    // Check if socket exists
    const socketExists = await stat(socketPath)
      .then(() => true)
      .catch(() => false);

    // Find tmux server process
    let serverProcess: string | undefined;
    try {
      const { stdout } = await execAsync("ps aux | grep 'tmux.*-L loom' | grep -v grep");
      serverProcess = stdout.trim();
    } catch {
      serverProcess = undefined;
    }

    // Get tmux version
    let tmuxVersion: string | undefined;
    try {
      const { stdout } = await execAsync("tmux -V");
      tmuxVersion = stdout.trim();
    } catch {
      tmuxVersion = undefined;
    }

    return {
      serverProcess,
      socketPath,
      socketExists,
      tmuxVersion,
    };
  } catch (error) {
    return {
      socketPath,
      socketExists: false,
      errorMessage: String(error),
    };
  }
}

/**
 * Toggle tmux verbose logging by sending SIGUSR2 to tmux server
 */
async function toggleTmuxVerboseLogging(): Promise<{
  success: boolean;
  message: string;
  pid?: string;
}> {
  try {
    // Find tmux server PID
    const { stdout } = await execAsync("pgrep -f 'tmux.*-L loom'");
    const pid = stdout.trim();

    if (!pid) {
      return {
        success: false,
        message: "tmux server not found (no process matching 'tmux.*-L loom')",
      };
    }

    // Send SIGUSR2 to toggle logging
    await execAsync(`kill -SIGUSR2 ${pid}`);

    return {
      success: true,
      message: `Sent SIGUSR2 to tmux server (PID ${pid}) - check for tmux-server-${pid}.log`,
      pid,
    };
  } catch (error) {
    return {
      success: false,
      message: `Error toggling tmux verbose logging: ${error}`,
    };
  }
}

/**
 * Generate a unique terminal ID
 */
async function generateTerminalId(): Promise<string> {
  const terminals = await listTerminals();
  const existingIds = terminals.map((t) => t.id);

  // Find the highest existing terminal number
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
 * Create a new terminal via the daemon
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
 * Delete (destroy) a terminal via the daemon
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
 * Restart a terminal by destroying and recreating it with the same config
 */
async function restartTerminal(terminalId: string): Promise<{
  success: boolean;
  terminal_id?: string;
  error?: string;
}> {
  try {
    // First, get the current terminal info
    const terminals = await listTerminals();
    const terminal = terminals.find((t) => t.id === terminalId);

    if (!terminal) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found`,
      };
    }

    // Store the config before destroying
    const savedConfig = {
      name: terminal.name,
      role: terminal.role,
      working_dir: terminal.working_dir,
    };

    // Destroy the terminal
    const destroyResult = await deleteTerminal(terminalId);
    if (!destroyResult.success) {
      return {
        success: false,
        error: `Failed to destroy terminal: ${destroyResult.error}`,
      };
    }

    // Wait a brief moment for cleanup to complete
    await new Promise((resolve) => setTimeout(resolve, 500));

    // Recreate the terminal with the same ID
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
 * Configuration options for updating a terminal
 */
interface ConfigureTerminalOptions {
  name?: string;
  role?: string;
  roleConfig?: Partial<RoleConfig>;
  theme?: string;
}

/**
 * Configure a terminal by updating its settings in the config file
 */
async function configureTerminal(
  terminalId: string,
  options: ConfigureTerminalOptions
): Promise<{
  success: boolean;
  error?: string;
}> {
  try {
    // Validate that at least one option is provided
    if (!options.name && !options.role && !options.roleConfig && !options.theme) {
      return {
        success: false,
        error: "At least one configuration option must be provided",
      };
    }

    // Read the current config
    const config = await readConfigFile();
    if (!config) {
      return {
        success: false,
        error: "Config file not found. Workspace may not be initialized.",
      };
    }

    // Find the terminal in the config
    const terminalIndex = config.terminals.findIndex((t) => t.id === terminalId);
    if (terminalIndex === -1) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found in config`,
      };
    }

    const terminal = config.terminals[terminalIndex];

    // Update the terminal configuration
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

    // Write the updated config back
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
 * Set the primary (selected) terminal
 */
async function setPrimaryTerminal(terminalId: string): Promise<{
  success: boolean;
  error?: string;
}> {
  try {
    // Verify the terminal exists in the daemon
    const terminals = await listTerminals();
    const terminal = terminals.find((t) => t.id === terminalId);

    if (!terminal) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found`,
      };
    }

    // Read current state or create new one
    let state = await readStateFile();
    if (!state) {
      // Create a new state file with the current terminals
      state = {
        terminals: terminals.map((t) => ({
          id: t.id,
          name: t.name,
          role: t.role,
          working_dir: t.working_dir,
          tmux_session: t.tmux_session,
          created_at: Date.now(),
          isPrimary: t.id === terminalId,
        })),
        selectedTerminalId: terminalId,
        lastUpdated: new Date().toISOString(),
      };
    } else {
      // Update isPrimary on all terminals - only the target gets true
      for (const t of state.terminals) {
        t.isPrimary = t.id === terminalId;
      }
      // Also update selectedTerminalId for backward compatibility
      state.selectedTerminalId = terminalId;
    }

    // Write the state file
    await writeStateFile(state);

    return { success: true };
  } catch (error) {
    return {
      success: false,
      error: `Error setting primary terminal: ${error}`,
    };
  }
}

/**
 * Clear terminal history (scrollback buffer and output log)
 */
async function clearTerminalHistory(terminalId: string): Promise<{
  success: boolean;
  error?: string;
}> {
  try {
    // Get the terminal info to find the tmux session name
    const terminals = await listTerminals();
    const terminal = terminals.find((t) => t.id === terminalId);

    if (!terminal) {
      return {
        success: false,
        error: `Terminal ${terminalId} not found`,
      };
    }

    // Clear tmux scrollback history
    try {
      await execAsync(`tmux -L loom clear-history -t "${terminal.tmux_session}"`);
    } catch (error) {
      // Session might not exist, but we can still try to clear the log file
      const stderr = (error as { stderr?: string }).stderr || "";
      if (!stderr.includes("no server running") && !stderr.includes("session not found")) {
        // Log warning but continue to clear the output file
      }
    }

    // Truncate the output log file
    const outputFile = `/tmp/loom-${terminalId}.out`;
    try {
      await writeFile(outputFile, "", "utf-8");
    } catch (error) {
      // File might not exist, which is fine
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
        return {
          success: false,
          error: `Failed to clear output log file: ${error}`,
        };
      }
    }

    return { success: true };
  } catch (error) {
    return {
      success: false,
      error: `Error clearing terminal history: ${error}`,
    };
  }
}

/**
 * Write MCP command to control file for Loom to pick up with retry and exponential backoff
 * This is a file-based IPC mechanism with acknowledgment
 */
async function writeMCPCommand(command: string): Promise<string> {
  // Ensure .loom directory exists
  try {
    await mkdir(LOOM_DIR, { recursive: true });
  } catch (_error) {
    // Directory might already exist, that's fine
  }

  // Clean up old acknowledgment file before writing new command
  try {
    await rm(MCP_ACK_FILE);
  } catch (_error) {
    // Ack file might not exist, that's fine
  }

  // Write command with timestamp
  const commandData = {
    command,
    timestamp: new Date().toISOString(),
  };

  await writeFile(MCP_COMMAND_FILE, JSON.stringify(commandData, null, 2));

  // Retry with exponential backoff to wait for acknowledgment
  const maxRetries = 8; // Max 8 retries
  const baseDelay = 100; // Start with 100ms
  const maxDelay = 5000; // Cap at 5 seconds
  let totalWaitTime = 0;

  for (let attempt = 0; attempt < maxRetries; attempt++) {
    // Calculate exponential backoff delay: 100ms, 200ms, 400ms, 800ms, 1600ms, 3200ms, 5000ms, 5000ms
    const delay = Math.min(baseDelay * 2 ** attempt, maxDelay);
    totalWaitTime += delay;

    // Wait before checking for acknowledgment
    await new Promise((resolve) => setTimeout(resolve, delay));

    // Check if acknowledgment file exists
    try {
      await stat(MCP_ACK_FILE);

      // Read acknowledgment data
      const ackContent = await readFile(MCP_ACK_FILE, "utf-8");
      const ackData = JSON.parse(ackContent);

      // Verify the ack is for our command
      if (ackData.command === command && ackData.timestamp === commandData.timestamp) {
        // Clean up acknowledgment file
        try {
          await rm(MCP_ACK_FILE);
        } catch (_error) {
          // Ignore cleanup errors
        }

        if (ackData.success) {
          return `MCP command '${command}' processed successfully (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        } else {
          return `MCP command '${command}' acknowledged but execution failed (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        }
      }
    } catch (_error) {
      // Ack file doesn't exist yet or couldn't be read, continue retrying
    }
  }

  // Max retries exceeded - give up but don't error
  return `MCP command '${command}' written but no acknowledgment received after ${maxRetries} retries (${totalWaitTime}ms total). The command may still be processing.`;
}

/**
 * Start autonomous mode for all configured terminals
 */
async function startAutonomousMode(): Promise<{
  success: boolean;
  message: string;
}> {
  try {
    const result = await writeMCPCommand("start_autonomous_mode");
    return {
      success: result.includes("successfully"),
      message: result,
    };
  } catch (error) {
    return {
      success: false,
      message: `Error starting autonomous mode: ${error}`,
    };
  }
}

/**
 * Stop autonomous mode for all terminals
 */
async function stopAutonomousMode(): Promise<{
  success: boolean;
  message: string;
}> {
  try {
    const result = await writeMCPCommand("stop_autonomous_mode");
    return {
      success: result.includes("successfully"),
      message: result,
    };
  } catch (error) {
    return {
      success: false,
      message: `Error stopping autonomous mode: ${error}`,
    };
  }
}

/**
 * Manually trigger the interval prompt for a specific terminal
 */
async function launchInterval(terminalId: string): Promise<{
  success: boolean;
  message: string;
}> {
  try {
    const result = await writeMCPCommand(`launch_interval:${terminalId}`);
    return {
      success: result.includes("successfully"),
      message: result,
    };
  } catch (error) {
    return {
      success: false,
      message: `Error launching interval: ${error}`,
    };
  }
}

/**
 * Agent metrics result structure
 */
interface AgentMetricsResult {
  success: boolean;
  data?: unknown;
  error?: string;
  format: "json" | "text";
  output: string;
}

/**
 * Get agent performance metrics via CLI script
 *
 * Enables agents to query their own performance metrics for self-aware behavior.
 * Part of Phase 5 (Autonomous Learning) - Issue #1073.
 */
async function getAgentMetrics(options: {
  command?: string;
  role?: string;
  period?: string;
  format?: string;
  issue?: number;
}): Promise<AgentMetricsResult> {
  const workspacePath = getWorkspacePath();
  const scriptPath = join(workspacePath, ".loom", "scripts", "agent-metrics.sh");

  try {
    // Check if script exists
    await stat(scriptPath);
  } catch {
    return {
      success: false,
      error: `Agent metrics script not found at ${scriptPath}. Ensure Loom is installed.`,
      format: "text",
      output: "",
    };
  }

  // Build command arguments
  const args: string[] = [];

  if (options.command && options.command !== "summary") {
    args.push(options.command);
  }

  if (options.role) {
    args.push("--role", options.role);
  }

  if (options.period) {
    args.push("--period", options.period);
  }

  const format = options.format || "json";
  args.push("--format", format);

  if (options.issue) {
    args.push("--issue", String(options.issue));
  }

  try {
    const { stdout, stderr } = await execAsync(`bash "${scriptPath}" ${args.join(" ")}`, {
      cwd: workspacePath,
    });

    if (stderr) {
      console.error("agent-metrics.sh stderr:", stderr);
    }

    // Parse JSON output if format is json
    let data: unknown;
    if (format === "json") {
      try {
        data = JSON.parse(stdout.trim());
      } catch {
        // If JSON parse fails, return as text
        return {
          success: true,
          output: stdout.trim(),
          format: "text",
        };
      }
    }

    return {
      success: true,
      data,
      output: stdout.trim(),
      format: format as "json" | "text",
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

const server = new Server(
  {
    name: "loom-terminals",
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

server.setRequestHandler(ListToolsRequestSchema, async () => {
  return {
    tools: [
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
        name: "get_selected_terminal",
        description:
          "Get information about the currently selected (primary) terminal in Loom. Returns the terminal's ID, name, role, and recent output.",
        inputSchema: {
          type: "object",
          properties: {
            lines: {
              type: "number",
              description: "Number of output lines to include (default: 20)",
              default: 20,
            },
          },
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
        name: "check_tmux_server_health",
        description:
          "Check if tmux server is running and count active loom sessions. Use this to verify tmux server status and detect crashes.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "get_tmux_server_info",
        description:
          "Get tmux server information including PID, socket path, and version. Use this to diagnose tmux server issues and verify socket paths.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "toggle_tmux_verbose_logging",
        description:
          "Toggle tmux verbose logging by sending SIGUSR2 to the tmux server. Creates tmux-server-{PID}.log file. Use this for deep debugging of tmux issues.",
        inputSchema: {
          type: "object",
          properties: {},
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
              description:
                "Working directory for the terminal (optional, defaults to workspace root)",
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
        name: "set_primary_terminal",
        description:
          "Set the primary (selected) terminal in the Loom UI. This updates the UI state to focus on the specified terminal. The terminal must exist.",
        inputSchema: {
          type: "object",
          properties: {
            terminal_id: {
              type: "string",
              description: "Terminal ID to set as primary (e.g., 'terminal-1')",
            },
          },
          required: ["terminal_id"],
        },
      },
      {
        name: "clear_terminal_history",
        description:
          "Clear a terminal's scrollback history and output log. This clears the tmux scrollback buffer and truncates the output log file. Useful for clearing sensitive data or resetting terminal state without restarting.",
        inputSchema: {
          type: "object",
          properties: {
            terminal_id: {
              type: "string",
              description: "Terminal ID to clear history for (e.g., 'terminal-1')",
            },
          },
          required: ["terminal_id"],
        },
      },
      {
        name: "start_autonomous_mode",
        description:
          "Start autonomous mode for all configured terminals. This starts the interval prompt timers for all terminals that have a targetInterval configured. Terminals will receive their intervalPrompt at their configured intervals when idle.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "stop_autonomous_mode",
        description:
          "Stop autonomous mode for all terminals. This pauses all interval prompt timers, preventing automatic prompts from being sent. Terminals remain running but won't receive autonomous prompts until autonomous mode is started again.",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
      {
        name: "launch_interval",
        description:
          "Manually trigger the interval prompt for a specific terminal immediately. This sends the terminal's configured intervalPrompt without waiting for the next scheduled interval. Works regardless of whether autonomous mode is enabled. Useful for testing autonomous behavior or manually triggering work.",
        inputSchema: {
          type: "object",
          properties: {
            terminal_id: {
              type: "string",
              description: "Terminal ID to trigger the interval prompt for (e.g., 'terminal-1')",
            },
          },
          required: ["terminal_id"],
        },
      },
      {
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
    ],
  };
});

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    switch (name) {
      case "list_terminals": {
        const terminals = await listTerminals();

        if (terminals.length === 0) {
          return {
            content: [
              {
                type: "text",
                text: "No active terminals found. Either Loom hasn't been started yet, or all terminals have been closed.",
              },
            ],
          };
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
            return `• ${parts}`;
          })
          .join("\n\n");

        return {
          content: [
            {
              type: "text",
              text: `=== Active Loom Terminals (${terminals.length}) ===\n\n${terminalList}`,
            },
          ],
        };
      }

      case "get_terminal_output": {
        const terminalId = args?.terminal_id as string;
        const lines = (args?.lines as number) || 20;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
          };
        }

        const result = await getTerminalOutput(terminalId, lines);
        return {
          content: [
            {
              type: "text",
              text: formatTerminalOutput(result, terminalId),
            },
          ],
        };
      }

      case "get_selected_terminal": {
        const lines = (args?.lines as number) || 20;
        const state = await readStateFile();

        if (!state || !state.selectedTerminalId) {
          return {
            content: [
              {
                type: "text",
                text: "No terminal is currently selected in Loom.",
              },
            ],
          };
        }

        const terminal = state.terminals.find((t) => t.id === state.selectedTerminalId);

        if (!terminal) {
          return {
            content: [
              {
                type: "text",
                text: `Selected terminal ${state.selectedTerminalId} not found in state file.`,
              },
            ],
          };
        }

        const result = await getTerminalOutput(terminal.id, lines);

        const info = [
          `ID: ${terminal.id}`,
          `Name: ${terminal.name}`,
          terminal.role ? `Role: ${terminal.role}` : null,
          terminal.working_dir ? `Working Dir: ${terminal.working_dir}` : null,
          `Session: ${terminal.tmux_session}`,
        ]
          .filter(Boolean)
          .join("\n");

        return {
          content: [
            {
              type: "text",
              text: `=== Currently Selected Terminal ===\n\n${info}\n\n${formatTerminalOutput(result, terminal.id)}`,
            },
          ],
        };
      }

      case "send_terminal_input": {
        const terminalId = args?.terminal_id as string;
        const input = args?.input as string;

        if (!terminalId || !input) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id and input are required",
              },
            ],
          };
        }

        const result = await sendTerminalInput(terminalId, input);
        return {
          content: [
            {
              type: "text",
              text: result,
            },
          ],
        };
      }

      case "check_tmux_server_health": {
        const health = await checkTmuxServerHealth();

        if (!health.serverRunning) {
          return {
            content: [
              {
                type: "text",
                text: `=== tmux Server Health ===\n\n🚨 Server Status: NOT RUNNING\n\nError: ${health.errorMessage || "Server not responding"}\n\nThis usually means:\n- tmux server crashed\n- No tmux sessions have been created yet\n- Socket path issue\n\nTo start the server, create a new terminal or run:\n  tmux -L loom new-session -d`,
              },
            ],
          };
        }

        const sessionList = health.sessions.map((s) => `  - ${s}`).join("\n");
        return {
          content: [
            {
              type: "text",
              text: `=== tmux Server Health ===\n\n✅ Server Status: RUNNING\nSession Count: ${health.sessionCount}\n\nActive loom sessions:\n${sessionList || "  (none)"}`,
            },
          ],
        };
      }

      case "get_tmux_server_info": {
        const info = await getTmuxServerInfo();

        let statusText = `=== tmux Server Information ===\n\n`;
        statusText += `Socket Path: ${info.socketPath}\n`;
        statusText += `Socket Exists: ${info.socketExists ? "✅ Yes" : "❌ No"}\n\n`;

        if (info.tmuxVersion) {
          statusText += `tmux Version: ${info.tmuxVersion}\n\n`;
        }

        if (info.serverProcess) {
          statusText += `Server Process:\n${info.serverProcess}\n`;
        } else {
          statusText += `Server Process: ❌ Not found (no matching process)\n`;
        }

        return {
          content: [
            {
              type: "text",
              text: statusText,
            },
          ],
        };
      }

      case "toggle_tmux_verbose_logging": {
        const result = await toggleTmuxVerboseLogging();

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Toggle tmux Verbose Logging ===\n\n❌ Failed\n\n${result.message}`,
              },
            ],
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Toggle tmux Verbose Logging ===\n\n✅ Success\n\n${result.message}\n\nNote: Verbose logging writes to tmux-server-${result.pid}.log in the current directory where the tmux server was started.`,
            },
          ],
        };
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
          return {
            content: [
              {
                type: "text",
                text: `=== Create Terminal ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
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

        return {
          content: [
            {
              type: "text",
              text: `=== Create Terminal ===\n\n✅ Success\n\n${details}\n\nThe terminal has been created with its own tmux session. You can now send commands to it using send_terminal_input.`,
            },
          ],
        };
      }

      case "delete_terminal": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        const result = await deleteTerminal(terminalId);

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Delete Terminal ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Delete Terminal ===\n\n✅ Success\n\nTerminal ${terminalId} has been deleted.\n\nCleanup performed:\n- tmux session killed\n- Output log file removed\n- Worktree cleaned up (if not used by other terminals)`,
            },
          ],
        };
      }

      case "restart_terminal": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        const result = await restartTerminal(terminalId);

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Restart Terminal ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Restart Terminal ===\n\n✅ Success\n\nTerminal ${result.terminal_id} has been restarted.\n\nThe terminal's configuration (ID, name, role, working directory) was preserved, but a fresh tmux session was created.\n\nNote: Any running processes in the terminal were terminated and terminal history was cleared.`,
            },
          ],
        };
      }

      case "configure_terminal": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        // Build the options object from the provided arguments
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
          return {
            content: [
              {
                type: "text",
                text: `=== Configure Terminal ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
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

        return {
          content: [
            {
              type: "text",
              text: `=== Configure Terminal ===\n\n✅ Success\n\nTerminal ${terminalId} configuration updated.\n\nUpdated fields: ${changedFields}\n\nNote: Changes are saved to the config file. The terminal may need to be restarted for some changes to take effect, or the UI will hot-reload the configuration.`,
            },
          ],
        };
      }

      case "set_primary_terminal": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        const result = await setPrimaryTerminal(terminalId);

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Set Primary Terminal ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Set Primary Terminal ===\n\n✅ Success\n\nTerminal ${terminalId} is now the primary (selected) terminal.\n\nThe UI will focus on this terminal.`,
            },
          ],
        };
      }

      case "clear_terminal_history": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        const result = await clearTerminalHistory(terminalId);

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Clear Terminal History ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Clear Terminal History ===\n\n✅ Success\n\nTerminal ${terminalId} history has been cleared.\n\nCleared:\n- tmux scrollback buffer\n- Output log file (/tmp/loom-${terminalId}.out)`,
            },
          ],
        };
      }

      case "start_autonomous_mode": {
        const result = await startAutonomousMode();

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Start Autonomous Mode ===\n\n❌ Failed\n\n${result.message}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Start Autonomous Mode ===\n\n✅ Success\n\nAutonomous mode has been started for all configured terminals.\n\nTerminals with targetInterval > 0 will now receive their intervalPrompt when idle.\n\n${result.message}`,
            },
          ],
        };
      }

      case "stop_autonomous_mode": {
        const result = await stopAutonomousMode();

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Stop Autonomous Mode ===\n\n❌ Failed\n\n${result.message}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Stop Autonomous Mode ===\n\n✅ Success\n\nAutonomous mode has been stopped for all terminals.\n\nTerminals will no longer receive automatic interval prompts until autonomous mode is started again.\n\n${result.message}`,
            },
          ],
        };
      }

      case "launch_interval": {
        const terminalId = args?.terminal_id as string;

        if (!terminalId) {
          return {
            content: [
              {
                type: "text",
                text: "Error: terminal_id is required",
              },
            ],
            isError: true,
          };
        }

        const result = await launchInterval(terminalId);

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Launch Interval ===\n\n❌ Failed\n\n${result.message}`,
              },
            ],
            isError: true,
          };
        }

        return {
          content: [
            {
              type: "text",
              text: `=== Launch Interval ===\n\n✅ Success\n\nInterval prompt triggered for terminal ${terminalId}.\n\nThe terminal's configured intervalPrompt has been sent.\n\n${result.message}`,
            },
          ],
        };
      }

      case "get_agent_metrics": {
        const command = (args?.command as string) || "summary";
        const role = args?.role as string | undefined;
        const period = (args?.period as string) || "week";
        const format = (args?.format as string) || "json";
        const issue = args?.issue as number | undefined;

        const result = await getAgentMetrics({
          command,
          role,
          period,
          format,
          issue,
        });

        if (!result.success) {
          return {
            content: [
              {
                type: "text",
                text: `=== Agent Metrics ===\n\n❌ Failed\n\n${result.error}`,
              },
            ],
            isError: true,
          };
        }

        // Format the output nicely
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

        return {
          content: [
            {
              type: "text",
              text: `${header}${filterInfo ? filterInfo + "\n\n" : "\n"}${output}`,
            },
          ],
        };
      }

      default:
        return {
          content: [
            {
              type: "text",
              text: `Unknown tool: ${name}`,
            },
          ],
          isError: true,
        };
    }
  } catch (error) {
    return {
      content: [
        {
          type: "text",
          text: `Error: ${error}`,
        },
      ],
      isError: true,
    };
  }
});

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("Loom Terminals MCP server running on stdio");
}

main().catch((error) => {
  console.error("Fatal error in main():", error);
  process.exit(1);
});
