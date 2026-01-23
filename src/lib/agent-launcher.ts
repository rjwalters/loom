import { invoke } from "@tauri-apps/api/core";
import { Logger } from "./logger";
import { detectTerminalState, type TerminalState } from "./terminal-state-parser";
import {
  TERMINAL_OUTPUT_STABILIZATION_MS,
  TERMINAL_POLL_INTERVAL_MS,
  WORKER_INITIALIZATION_DELAY_MS,
} from "./timing-constants";

const logger = Logger.forComponent("agent-launcher");

/**
 * agent-launcher.ts - Functions for launching AI agents in terminals
 *
 * IMPORTANT: All functions in this module operate on sessionIds (ephemeral tmux session IDs).
 * - terminalId parameters are sessionIds used for IPC operations (send_input, etc.)
 * - Callers should use configId for state management, look up sessionId for launching
 */

/**
 * Detect what type of process is running in a terminal (passive detection)
 *
 * Uses passive detection by reading and parsing terminal output instead of
 * sending probe commands. This approach is:
 * - Non-intrusive: No commands sent to terminal
 * - Real-time: Can detect state immediately
 * - More informative: Can detect multiple states (working, paused, waiting, etc.)
 *
 * This is useful for:
 * - Verifying an agent was launched successfully
 * - Detecting bypass permissions prompt
 * - Distinguishing between shells and agents
 * - Debugging agent launch issues
 *
 * @param terminalId - The terminal ID to check
 * @param lineCount - Number of lines to analyze (default: 20)
 * @returns Promise resolving to the terminal state with detected type and status
 */
export async function detectTerminalType(
  terminalId: string,
  lineCount = 20
): Promise<TerminalState> {
  logger.info("Detecting terminal type with passive parsing", { terminalId, lineCount });

  const state = await detectTerminalState(terminalId, lineCount);

  logger.info("Terminal type detection complete", {
    terminalId,
    type: state.type,
    status: state.status,
    hasPrompt: !!state.lastPrompt,
  });

  return state;
}

/**
 * Launch a Claude agent in a terminal by sending the Claude CLI command
 *
 * This uses the existing terminal input mechanism to send a Claude command
 * with the appropriate role prompt and configuration. The agent runs visibly
 * in the terminal where users can see output and interact if needed.
 *
 * NOTE: Worktrees are now created on-demand when claiming issues, not automatically.
 * Agents start in the main workspace and create worktrees using `pnpm worktree <issue>`.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path (empty string if agent should use main workspace)
 * @returns Promise that resolves when the agent is launched
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  logger.info("Starting agent launch", {
    terminalId,
    roleFile,
    workspacePath,
    worktreePath: worktreePath || "(main workspace)",
  });

  // Use worktree path if provided, otherwise use main workspace
  const agentWorkingDir = worktreePath || workspacePath;
  const location = worktreePath ? "worktree" : "main workspace";
  logger.info(`Agent will start in ${location}`, {
    terminalId,
    agentWorkingDir,
  });

  // Read role file content from workspace
  logger.info("Reading role file", { terminalId, roleFile });
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  logger.info("Role file read successfully", {
    terminalId,
    roleFile,
    contentLength: roleContent.length,
  });

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);
  logger.info("Processed role prompt", { terminalId, workspaceReplaced: agentWorkingDir });

  // Build Claude CLI command
  // Note: We send the role as the first message instead of writing CLAUDE.md
  // This prevents conflicts with the main workspace CLAUDE.md (project instructions)
  // Using --dangerously-skip-permissions to bypass the interactive warning prompt
  const command = "claude --dangerously-skip-permissions";
  logger.info("Sending Claude command to terminal", { terminalId, command });

  // Wait for any previous commands to fully complete
  // This prevents command concatenation with worktree setup commands
  logger.info("Waiting for previous commands to complete", {
    terminalId,
    delayMs: TERMINAL_OUTPUT_STABILIZATION_MS,
  });
  await new Promise((resolve) => setTimeout(resolve, TERMINAL_OUTPUT_STABILIZATION_MS));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  logger.info("Command sent to terminal", { terminalId });

  // Press Enter to execute
  logger.info("Sending Enter to execute command", { terminalId });
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Poll for bypass permissions prompt using passive detection
  // Instead of blindly sending "2", detect the prompt first, then respond
  const maxAttempts = 10; // Max polling attempts
  const pollInterval = 1000; // Check every 1 second
  let bypassAccepted = false;

  logger.info("Polling for bypass permissions prompt", { terminalId, maxAttempts, pollInterval });

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, pollInterval));

    // Check terminal state using passive detection
    const state = await detectTerminalType(terminalId, 10);

    logger.info("Bypass polling attempt", {
      terminalId,
      attempt,
      detectedType: state.type,
      detectedStatus: state.status,
    });

    if (state.status === "bypass-prompt") {
      // Bypass prompt detected - send "2" to accept
      logger.info("Bypass prompt detected, accepting", { terminalId, attempt });

      await invoke("send_terminal_input", {
        id: terminalId,
        data: "2",
      });

      // Small delay before Enter
      await new Promise((resolve) => setTimeout(resolve, TERMINAL_POLL_INTERVAL_MS));

      await invoke("send_terminal_input", {
        id: terminalId,
        data: "\r",
      });

      bypassAccepted = true;
      logger.info("Bypass permissions accepted", { terminalId });
      break;
    } else if (state.status === "waiting-input") {
      // Agent already ready, no bypass prompt needed
      logger.info("Agent already ready, skipping bypass", { terminalId, attempt });
      bypassAccepted = true;
      break;
    }

    // Continue polling
  }

  if (!bypassAccepted) {
    logger.error(
      "Bypass permissions prompt not detected within timeout",
      new Error("Timeout waiting for bypass prompt"),
      { terminalId, maxAttempts }
    );
  }

  // Wait for Claude Code to be ready to receive input and send role prompt
  // Wrapped in try-catch to capture any silent failures
  try {
    logger.info("Waiting for Claude Code to initialize", {
      terminalId,
      delayMs: WORKER_INITIALIZATION_DELAY_MS,
    });
    await new Promise((resolve) => setTimeout(resolve, WORKER_INITIALIZATION_DELAY_MS));
    logger.info("Wait complete, proceeding to send role prompt", { terminalId });

    // Send the role prompt as the first message
    logger.info("Sending role prompt as first message", {
      terminalId,
      promptLength: processedPrompt.length,
    });
    await invoke("send_terminal_input", {
      id: terminalId,
      data: processedPrompt,
    });
    logger.info("Role prompt sent successfully", { terminalId });

    // Press Enter to submit the prompt
    logger.info("Sending Enter to submit role prompt", { terminalId });
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "\r",
    });
    logger.info("Enter sent successfully", { terminalId });
  } catch (error) {
    logger.error("Failed to send role prompt or Enter", error as Error, {
      terminalId,
      promptLength: processedPrompt.length,
    });
    throw error; // Re-throw to let caller handle
  }

  // Wait for the agent to process the role prompt
  logger.info("Waiting for agent to process role prompt", {
    terminalId,
    delayMs: WORKER_INITIALIZATION_DELAY_MS,
  });
  await new Promise((resolve) => setTimeout(resolve, WORKER_INITIALIZATION_DELAY_MS));

  // Verify agent launched successfully using passive detection
  logger.info("Verifying agent launch with passive detection", { terminalId });
  const state = await detectTerminalType(terminalId, 20);

  if (state.type === "claude-code" && state.status === "waiting-input") {
    logger.info("Agent launch verified successfully", {
      terminalId,
      type: state.type,
      status: state.status,
    });
  } else if (state.type === "shell") {
    logger.error(
      "Agent launch verification failed: detected shell instead of agent",
      new Error("Shell detected after agent launch"),
      { terminalId, detectedState: state }
    );
  } else {
    logger.error(
      "Agent launch verification inconclusive: unexpected state",
      new Error("Unexpected terminal state detected"),
      { terminalId, type: state.type, status: state.status }
    );
  }

  logger.info("Agent launch complete", { terminalId, agentWorkingDir });
}

/**
 * Stop a Claude agent running in a terminal by sending Ctrl+C
 *
 * @param terminalId - The terminal ID to stop the agent in
 * @returns Promise that resolves when the stop signal is sent
 */
export async function stopAgentInTerminal(terminalId: string): Promise<void> {
  // Send Ctrl+C (character code 3)
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\u0003",
  });
}

/**
 * Send a prompt to an agent running in a terminal
 *
 * This is used for autonomous mode to send interval prompts to agents.
 *
 * @param terminalId - The terminal ID to send the prompt to
 * @param prompt - The prompt text to send
 * @returns Promise that resolves when the prompt is sent
 */
export async function sendPromptToAgent(terminalId: string, prompt: string): Promise<void> {
  // Send the prompt text
  await invoke("send_terminal_input", {
    id: terminalId,
    data: prompt,
  });

  // Press Enter to submit
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch GitHub Copilot in a terminal
 *
 * This uses the gh CLI's copilot extension to start an interactive chat session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Copilot in
 * @returns Promise that resolves when Copilot is launched
 */
export async function launchGitHubCopilotAgent(terminalId: string): Promise<void> {
  // Build GitHub Copilot CLI command
  // Using 'gh copilot' for interactive chat mode
  const command = "gh copilot";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch Google Gemini CLI in a terminal
 *
 * This uses the Gemini CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Gemini in
 * @returns Promise that resolves when Gemini is launched
 */
export async function launchGeminiCLIAgent(terminalId: string): Promise<void> {
  // Build Gemini CLI command for interactive mode
  const command = "gemini chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch DeepSeek CLI in a terminal
 *
 * This uses the DeepSeek CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch DeepSeek in
 * @returns Promise that resolves when DeepSeek is launched
 */
export async function launchDeepSeekAgent(terminalId: string): Promise<void> {
  // Build DeepSeek CLI command for interactive mode
  const command = "deepseek chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch xAI Grok CLI in a terminal
 *
 * This uses the Grok CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Grok in
 * @returns Promise that resolves when Grok is launched
 */
export async function launchGrokAgent(terminalId: string): Promise<void> {
  // Build Grok CLI command for interactive mode
  const command = "grok chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch Sourcegraph Amp CLI in a terminal
 *
 * This uses the Amp CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Amp in
 * @returns Promise that resolves when Amp is launched
 */
export async function launchAmpAgent(terminalId: string): Promise<void> {
  // Build Amp CLI command for interactive mode
  const command = "amp";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch Codex agent in a terminal with system prompt
 *
 * This launches Codex with configuration similar to Claude Code:
 * - Loads role file and writes to CODEX.md (system prompt)
 * - Uses --full-auto for autonomous execution
 * - Workspace-write sandbox with on-failure approval
 * - Permissions configured in .codex/config.toml
 *
 * NOTE: Worktrees are now created on-demand when claiming issues, not automatically.
 * Agents start in the main workspace and create worktrees using `pnpm worktree <issue>`.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path (empty string if agent should use main workspace)
 * @returns Promise that resolves when the agent is launched
 */
export async function launchCodexAgent(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  logger.info("Starting Codex agent launch", {
    terminalId,
    roleFile,
    workspacePath,
    worktreePath: worktreePath || "(main workspace)",
  });

  // Use worktree path if provided, otherwise use main workspace
  const agentWorkingDir = worktreePath || workspacePath;
  const location = worktreePath ? "worktree" : "main workspace";
  logger.info(`Codex agent will start in ${location}`, {
    terminalId,
    agentWorkingDir,
  });

  // Read role file content from workspace
  logger.info("Reading role file for Codex", { terminalId, roleFile });
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  logger.info("Role file read successfully for Codex", {
    terminalId,
    roleFile,
    contentLength: roleContent.length,
  });

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);
  logger.info("Processed Codex role prompt", { terminalId, workspaceReplaced: agentWorkingDir });

  // Build Codex CLI command with autonomous configuration
  // Note: We send the prompt directly in the command instead of via a file
  // This prevents conflicts with files in the main workspace
  // --full-auto: Combines -a on-failure and --sandbox workspace-write
  // Additional config from .codex/config.toml (sandbox_permissions, etc.)
  // Using single quotes around the heredoc to prevent shell expansion
  const command = `codex --full-auto "$(cat <<'ROLE_EOF'\n${processedPrompt}\nROLE_EOF\n)"`;
  logger.info("Sending Codex command to terminal", {
    terminalId,
    promptLength: processedPrompt.length,
  });

  // Wait for any previous commands to fully complete
  logger.info("Waiting for previous commands to complete", {
    terminalId,
    delayMs: TERMINAL_OUTPUT_STABILIZATION_MS,
  });
  await new Promise((resolve) => setTimeout(resolve, TERMINAL_OUTPUT_STABILIZATION_MS));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  logger.info("Codex command sent to terminal", { terminalId });

  // Press Enter to execute
  logger.info("Sending Enter to execute Codex", { terminalId });
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Wait for the agent to initialize and process the role prompt
  logger.info("Waiting for Codex agent to initialize", {
    terminalId,
    delayMs: WORKER_INITIALIZATION_DELAY_MS,
  });
  await new Promise((resolve) => setTimeout(resolve, WORKER_INITIALIZATION_DELAY_MS));

  // Verify agent launched successfully using passive detection
  logger.info("Verifying Codex agent launch with passive detection", { terminalId });
  const state = await detectTerminalType(terminalId, 20);

  if (state.type === "codex") {
    logger.info("Codex agent launch verified successfully", {
      terminalId,
      type: state.type,
      status: state.status,
    });
  } else if (state.type === "shell") {
    logger.error(
      "Codex agent launch verification failed: detected shell instead of agent",
      new Error("Shell detected after Codex launch"),
      { terminalId, detectedState: state }
    );
  } else {
    logger.error(
      "Codex agent launch verification inconclusive: unexpected state",
      new Error("Unexpected terminal state detected after Codex launch"),
      { terminalId, type: state.type, status: state.status }
    );
  }

  logger.info("Codex agent launch complete", { terminalId, agentWorkingDir });
}
