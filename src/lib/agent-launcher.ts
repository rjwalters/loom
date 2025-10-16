import { invoke } from "@tauri-apps/api/tauri";
import { generateProbeCommand, parseProbeResponse, type TerminalType } from "./terminal-probe";

/**
 * agent-launcher.ts - Functions for launching AI agents in terminals
 *
 * IMPORTANT: All functions in this module operate on sessionIds (ephemeral tmux session IDs).
 * - terminalId parameters are sessionIds used for IPC operations (send_input, etc.)
 * - Callers should use configId for state management, look up sessionId for launching
 */

/**
 * Detect the type of terminal (bash shell vs AI agent) using intelligent probe command.
 *
 * This function:
 * 1. Sends a bash-compatible probe command to the terminal
 * 2. Waits for output
 * 3. Parses the response to determine terminal type
 *
 * The probe command is safe in both environments:
 * - In bash: Comment is ignored, `true` executes silently
 * - In AI agents: Comment is interpreted as a prompt, agent responds with structured info
 *
 * @param terminalId - The terminal ID to probe
 * @param waitMs - How long to wait for terminal response (default: 1000ms)
 * @returns Promise resolving to terminal type ('agent', 'shell', or 'unknown')
 *
 * @example
 * ```typescript
 * const type = await detectTerminalType('terminal-1');
 * if (type === 'agent') {
 *   console.log('Claude Code detected');
 * } else if (type === 'shell') {
 *   console.log('Plain bash shell detected');
 * }
 * ```
 */
export async function detectTerminalType(
  terminalId: string,
  waitMs = 1000
): Promise<TerminalType> {
  console.log(`[detectTerminalType] Probing ${terminalId}...`);

  // Generate probe command
  const probe = generateProbeCommand();
  console.log(`[detectTerminalType] Sending probe: ${probe.split("\n")[0]}...`);

  // Send probe to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: probe,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Wait for response
  console.log(`[detectTerminalType] Waiting ${waitMs}ms for response...`);
  await new Promise((resolve) => setTimeout(resolve, waitMs));

  // Read terminal output
  const terminalOutput = await invoke<{ output: string; byte_count: number }>(
    "get_terminal_output",
    {
      id: terminalId,
      start_byte: undefined, // Read all available output
    }
  );

  // Parse response
  const result = parseProbeResponse(terminalOutput.output);
  console.log(
    `[detectTerminalType] Detected type: ${result.type}${result.role ? ` (role: ${result.role})` : ""}`
  );

  return result.type;
}

/**
 * Launch a Claude agent in a terminal by sending the Claude CLI command
 *
 * This uses the existing terminal input mechanism to send a Claude command
 * with the appropriate role prompt and configuration. The agent runs visibly
 * in the terminal where users can see output and interact if needed.
 *
 * NOTE: All terminals now have worktrees automatically created during terminal creation.
 * This function expects worktreePath to be provided from terminal.worktreePath.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path for this terminal (required, created during terminal setup)
 * @returns Promise that resolves when the agent is launched
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  console.log(
    `[launchAgentInTerminal] START - terminalId=${terminalId}, roleFile=${roleFile}, workspacePath=${workspacePath}, worktreePath=${worktreePath}`
  );

  // Use the provided worktree path as the working directory
  const agentWorkingDir = worktreePath;
  console.log(`[launchAgentInTerminal] Using worktree at ${agentWorkingDir}`);

  // Read role file content from workspace
  console.log(`[launchAgentInTerminal] Reading role file ${roleFile}...`);
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  console.log(`[launchAgentInTerminal] Role file read successfully, length=${roleContent.length}`);

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);
  console.log(`[launchAgentInTerminal] Processed prompt with workspace=${agentWorkingDir}`);

  // Write the system prompt to CLAUDE.md in the worktree/workspace
  // Claude Code automatically loads CLAUDE.md from the repository root
  const claudeMdPath = `${agentWorkingDir}/CLAUDE.md`;
  console.log(`[launchAgentInTerminal] Writing CLAUDE.md to ${claudeMdPath}...`);
  await invoke("write_file", {
    path: claudeMdPath,
    content: processedPrompt,
  });
  console.log(`[launchAgentInTerminal] CLAUDE.md written successfully`);

  // Build Claude CLI command - CLAUDE.md will be automatically loaded
  // Note: --session-id removed because Claude Code requires UUID format, not our terminal IDs
  // Using --dangerously-skip-permissions to bypass the interactive warning prompt
  const command = "claude --dangerously-skip-permissions";
  console.log(`[launchAgentInTerminal] Sending command to terminal: ${command}`);

  // Wait for any previous commands to fully complete
  // This prevents command concatenation with worktree setup commands
  console.log(`[launchAgentInTerminal] Waiting 500ms for previous commands to complete...`);
  await new Promise((resolve) => setTimeout(resolve, 500));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  console.log(`[launchAgentInTerminal] Command sent`);

  // Press Enter to execute
  console.log(`[launchAgentInTerminal] Sending Enter to execute`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Add initial delay to allow Claude Code to start
  // This prevents the "2" from concatenating with the previous command
  console.log(`[launchAgentInTerminal] Initial wait: 1000ms for Claude Code to start...`);
  await new Promise((resolve) => setTimeout(resolve, 1000));

  // Retry accepting the bypass permissions warning with exponential backoff
  // Claude Code initialization time varies, so we try multiple times with increasing delays
  const maxRetries = 3;
  const baseDelay = 2000; // Base delay: 2 seconds

  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    const delay = baseDelay * attempt; // Exponential backoff: 2s, 4s, 6s
    console.log(
      `[launchAgentInTerminal] Attempt ${attempt}/${maxRetries}: Waiting ${delay}ms for bypass permissions prompt...`
    );
    await new Promise((resolve) => setTimeout(resolve, delay));

    // Accept the bypass permissions warning by selecting option 2
    console.log(`[launchAgentInTerminal] Attempt ${attempt}: Sending "2" to accept warning`);
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "2",
    });

    // Small delay before pressing Enter to ensure "2" is processed
    await new Promise((resolve) => setTimeout(resolve, 100));

    // Press Enter to confirm selection
    console.log(`[launchAgentInTerminal] Attempt ${attempt}: Sending Enter to confirm`);
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "\r",
    });

    // If this isn't the last attempt, wait a bit before retrying
    if (attempt < maxRetries) {
      console.log(`[launchAgentInTerminal] Waiting 500ms before next attempt...`);
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }

  console.log(`[launchAgentInTerminal] COMPLETE - agent launched in ${agentWorkingDir}`);
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
 * Launch Codex agent in a terminal with system prompt
 *
 * This launches Codex with configuration similar to Claude Code:
 * - Loads role file and writes to CODEX.md (system prompt)
 * - Uses --full-auto for autonomous execution
 * - Workspace-write sandbox with on-failure approval
 * - Permissions configured in .codex/config.toml
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path for this terminal
 * @returns Promise that resolves when the agent is launched
 */
export async function launchCodexAgent(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  console.log(
    `[launchCodexAgent] START - terminalId=${terminalId}, roleFile=${roleFile}, worktreePath=${worktreePath}`
  );

  // Read role file content from workspace
  console.log(`[launchCodexAgent] Reading role file ${roleFile}...`);
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  console.log(`[launchCodexAgent] Role file read successfully, length=${roleContent.length}`);

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, worktreePath);
  console.log(`[launchCodexAgent] Processed prompt with workspace=${worktreePath}`);

  // Write the system prompt to CODEX.md in the worktree
  // Codex will load this as the system prompt
  const codexMdPath = `${worktreePath}/CODEX.md`;
  console.log(`[launchCodexAgent] Writing CODEX.md to ${codexMdPath}...`);
  await invoke("write_file", {
    path: codexMdPath,
    content: processedPrompt,
  });
  console.log(`[launchCodexAgent] CODEX.md written successfully`);

  // Build Codex CLI command with autonomous configuration
  // --full-auto: Combines -a on-failure and --sandbox workspace-write
  // Additional config from .codex/config.toml (sandbox_permissions, etc.)
  const command = 'codex --full-auto "$(cat CODEX.md)"';
  console.log(`[launchCodexAgent] Sending command to terminal: ${command}`);

  // Wait for any previous commands to fully complete
  console.log(`[launchCodexAgent] Waiting 500ms for previous commands to complete...`);
  await new Promise((resolve) => setTimeout(resolve, 500));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  console.log(`[launchCodexAgent] Command sent`);

  // Press Enter to execute
  console.log(`[launchCodexAgent] Sending Enter to execute`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  console.log(`[launchCodexAgent] COMPLETE - Codex agent launched in ${worktreePath}`);
}
